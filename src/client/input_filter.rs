//! Filters terminal *reply* sequences out of the stdin byte stream.
//!
//! The client queries the outer terminal (OSC 10/11 palette probes, and
//! re-queries on behalf of the pane via TermOscQuery). Replies arriving
//! on stdin — including late ones after a query timed out — belong to
//! the client, not the pane: without this filter they are forwarded to
//! the child as keystrokes and appear as typed garbage.
//!
//! Sequences consumed: OSC (`ESC ] ... BEL|ST`), DCS (`ESC P ... ST`),
//! APC (`ESC _ ... ST`), PM (`ESC ^ ... ST`), SOS (`ESC X ... ST`), and
//! CSI reports whose final byte marks a device reply (R/c/n/t/x/y).
//! Everything else — including CSI keys (arrows, F-keys, `~`-final
//! sequences and bracketed-paste markers) — passes through untouched.
//!
//! A sequence split across reads is held briefly: if it completes as a
//! reply it is consumed, otherwise the bytes are released as input.

use std::time::{Duration, Instant};

/// How long a partial reply-looking sequence is held before releasing it
/// as input. Long enough for a fragmented terminal reply, short enough
/// that a stuck sequence does not swallow user typing.
const HOLD: Duration = Duration::from_millis(100);

/// CSI final bytes that denote a terminal → host report: primary/secondary
/// DA (c), DSR/status (n), window-ops replies (t), DECREQTPARM (x),
/// DECRPM (y), cursor-position report (R). Keys end in other finals
/// (A–Z keys, `~` for F-keys/bracketed paste, I/O for focus events).
const CSI_REPLY_FINALS: &[u8] = b"cntxyR";

#[derive(Default)]
pub struct InputFilter {
    /// Held bytes: a possible reply that has not completed yet.
    pending: Vec<u8>,
    /// When `pending` started accumulating (for the hold timeout).
    pending_since: Option<Instant>,
    /// Bytes recovered by out-of-band probes (OSC queries read past the
    /// reply). Re-scanned like fresh input, in case they embed a late
    /// reply themselves.
    injected: Vec<u8>,
    /// Bytes already classified as user input (expired holds). Emitted
    /// verbatim ahead of newly scanned bytes — never re-filtered.
    ready: Vec<u8>,
}

impl InputFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue bytes recovered by an out-of-band read (e.g. an OSC query
    /// that consumed user keystrokes alongside its reply). They are
    /// re-scanned — a late reply inside them is still filtered.
    pub fn inject(&mut self, bytes: &[u8]) {
        self.injected.extend_from_slice(bytes);
    }

    /// Bytes staged by `inject` or an expired hold are ready to process
    /// without waiting for stdin. The caller should run `feed(&[])` when
    /// this is true.
    pub fn has_staged(&self) -> bool {
        !self.injected.is_empty() || !self.ready.is_empty()
    }

    /// Consume a chunk of stdin bytes; returns what should reach the pane.
    pub fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(input.len() + self.pending.len() + self.injected.len());
        data.append(&mut self.injected);
        data.append(&mut self.pending);
        data.extend_from_slice(input);
        // hold() re-marks the timestamp if the scan holds again.
        self.pending_since = None;
        let mut out = std::mem::take(&mut self.ready);

        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b != 0x1b || i + 1 >= data.len() {
                out.push(b);
                i += 1;
                continue;
            }
            match data[i + 1] {
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    // String sequence: consume until BEL or ESC \.
                    match str_seq_end(&data[i..]) {
                        Some(end) => i += end,
                        None => {
                            self.hold(&data[i..]);
                            break;
                        }
                    }
                }
                b'[' => match csi_end(&data[i..]) {
                    Some((end, is_reply)) => {
                        if !is_reply {
                            out.extend_from_slice(&data[i..i + end]);
                        }
                        i += end;
                    }
                    None => {
                        self.hold(&data[i..]);
                        break;
                    }
                },
                // Lone ESC or ESC+key: user input — pass the ESC, keep
                // scanning the next byte normally.
                _ => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        out
    }

    /// Release held bytes once the hold timeout expires — a "reply" that
    /// never completed was almost certainly user input. The bytes are
    /// staged into `ready` and emitted by the next `feed`.
    pub fn flush_expired(&mut self) {
        if let Some(since) = self.pending_since
            && since.elapsed() >= HOLD
        {
            self.pending_since = None;
            self.ready.append(&mut self.pending);
        }
    }

    /// Milliseconds until the hold expires, for the caller's poll timeout.
    pub fn hold_remaining_ms(&self) -> Option<i32> {
        self.pending_since.map(|s| {
            HOLD.as_millis()
                .saturating_sub(s.elapsed().as_millis())
                .max(1) as i32
        })
    }

    fn hold(&mut self, bytes: &[u8]) {
        if self.pending_since.is_none() {
            self.pending_since = Some(Instant::now());
        }
        self.pending.extend_from_slice(bytes);
    }
}

/// Length of a complete string sequence starting at `ESC ]/P/_/^/X`,
/// terminated by BEL or `ESC \`. Returns its end offset (exclusive).
fn str_seq_end(data: &[u8]) -> Option<usize> {
    let mut i = 2; // skip ESC + opener
    while i < data.len() {
        if data[i] == 0x07 {
            return Some(i + 1);
        }
        if data[i] == 0x1b {
            return match data.get(i + 1) {
                Some(&b'\\') => Some(i + 2),
                // Trailing ESC: may be the first byte of a split ST —
                // hold it instead of aborting.
                None => None,
                // ESC not followed by '\' aborts the sequence — consume
                // up to it so the stray bytes do not leak to the pane.
                Some(_) => Some(i),
            };
        }
        i += 1;
    }
    None
}

/// Length of a complete CSI (`ESC [` params intermediates final) and
/// whether it is a terminal reply. Returns (end, is_reply).
fn csi_end(data: &[u8]) -> Option<(usize, bool)> {
    let mut i = 2; // skip ESC [
    while i < data.len() {
        let b = data[i];
        match b {
            0x20..=0x3f => i += 1, // params + intermediates
            0x40..=0x7e => return Some((i + 1, CSI_REPLY_FINALS.contains(&b))),
            _ => return Some((i, false)), // malformed — pass through
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(f: &mut InputFilter, s: &[u8]) -> String {
        String::from_utf8_lossy(&f.feed(s)).into_owned()
    }

    #[test]
    fn plain_input_passes_through() {
        let mut f = InputFilter::new();
        assert_eq!(feed(&mut f, b"hello\r"), "hello\r");
    }

    #[test]
    fn arrow_keys_pass_through() {
        let mut f = InputFilter::new();
        assert_eq!(
            feed(&mut f, b"\x1b[A\x1b[1;5D\x1b[Z"),
            "\x1b[A\x1b[1;5D\x1b[Z"
        );
    }

    #[test]
    fn bracketed_paste_markers_pass_through() {
        let mut f = InputFilter::new();
        assert_eq!(
            feed(&mut f, b"\x1b[200~text\x1b[201~"),
            "\x1b[200~text\x1b[201~"
        );
    }

    #[test]
    fn osc_reply_consumed_bel_and_st() {
        let mut f = InputFilter::new();
        assert_eq!(feed(&mut f, b"a\x1b]11;rgb:0000/0000/0000\x07b"), "ab");
        let mut f = InputFilter::new();
        assert_eq!(feed(&mut f, b"a\x1b]10;rgb:ffff/ffff/ffff\x1b\\b"), "ab");
    }

    #[test]
    fn csi_replies_consumed() {
        let mut f = InputFilter::new();
        // cursor position report, DA response, DECRPM
        assert_eq!(feed(&mut f, b"x\x1b[24;80Ry"), "xy");
        assert_eq!(feed(&mut f, b"\x1b[?1;2c"), "");
        assert_eq!(feed(&mut f, b"\x1b[?2026;2$y"), "");
    }

    #[test]
    fn split_reply_held_then_consumed() {
        let mut f = InputFilter::new();
        assert_eq!(feed(&mut f, b"pre\x1b]11;rgb:00"), "pre");
        // Completes on the next read.
        assert_eq!(feed(&mut f, b"00/0000\x07post"), "post");
    }

    #[test]
    fn incomplete_sequence_released_on_timeout() {
        let mut f = InputFilter::new();
        assert_eq!(feed(&mut f, b"x\x1b]9;"), "x");
        f.pending_since = Some(Instant::now() - HOLD - Duration::from_millis(1));
        f.flush_expired();
        assert!(f.has_staged());
        assert_eq!(feed(&mut f, b""), "\x1b]9;");
    }

    #[test]
    fn injected_bytes_rescanned() {
        let mut f = InputFilter::new();
        f.inject(b"user\x1b]11;rgb:1/2/3\x07keys");
        assert!(f.has_staged());
        assert_eq!(feed(&mut f, b""), "userkeys");
    }
}
