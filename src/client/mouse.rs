//! Mouse report decoding (terminal → client) and re-encoding (client → pane).
//!
//! When the child enables mouse tracking (DECSET 1000/1002/1003), the client
//! enables reporting on the outer terminal and forwards each event as
//! `PaneInput`, re-encoded in the format the child requested: legacy X10,
//! UTF-8 extended (1005) or SGR (1006).
//!
//! The decoder understands X10 and SGR reports regardless of the child's
//! format — SGR is requested from the outer terminal, but terminals that
//! ignore mode 1006 fall back to X10 and still parse correctly here.

use std::time::{Duration, Instant};

/// A decoded mouse event. `cb` is the button byte as defined by X10/SGR:
/// bits 0-1 button (0=left, 1=middle, 2=right, 3=release), bit 2 Shift,
/// bit 3 Alt, bit 4 Ctrl, bit 5 motion, bit 6 wheel. `release` is set for
/// SGR `m`-terminated reports (button release). `x`/`y` are 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub cb: u8,
    pub x: u16,
    pub y: u16,
    pub release: bool,
}

impl Event {
    /// Wheel event (cb bit 6): 64/65 = vertical up/down, 66/67 = horizontal.
    pub fn is_wheel(&self) -> bool {
        self.cb & 0x40 != 0
    }

    /// Drag or any-motion event (cb bit 5).
    pub fn is_motion(&self) -> bool {
        self.cb & 0x20 != 0
    }

    /// Wheel-up direction (scrolls toward scrollback).
    pub fn is_wheel_up(&self) -> bool {
        self.is_wheel() && self.cb & 1 == 0
    }
}

/// Streaming decoder: feed raw stdin bytes, get back the bytes that are not
/// mouse reports plus any complete events. Partial sequences at the end of a
/// chunk are buffered until the rest arrives (with a short hold — a lone
/// Esc keypress is also just `0x1b`, so incomplete sequences are released
/// after `HOLD` rather than swallowed).
#[derive(Default)]
pub struct Decoder {
    /// Held bytes: a possible report that has not completed yet.
    pending: Vec<u8>,
    /// When `pending` started accumulating (for the hold timeout).
    pending_since: Option<Instant>,
    /// Bytes released by an expired hold — emitted verbatim ahead of
    /// newly scanned bytes, never re-scanned (avoids re-holding a lone
    /// Esc forever).
    ready: Vec<u8>,
    /// Whether the outer terminal currently reports mouse events. When
    /// false, feed() is a pass-through — no holding, zero added latency.
    active: bool,
}

/// How long an incomplete report-looking sequence is held waiting for its
/// tail. Reports are written atomically by the terminal — splits span
/// consecutive reads — so this only needs to cover one poll cycle. It is
/// also the delay a bare Esc sees while mouse reporting is on.
const HOLD: Duration = Duration::from_millis(50);

/// Safety cap: a report-looking prefix that never completes is released as
/// input instead of eating keystrokes forever.
const MAX_PENDING: usize = 64;

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable/disable decoding. When disabled, buffered bytes are flushed
    /// and input passes through untouched.
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// Bytes staged by an expired hold are ready to emit without waiting
    /// for stdin. The caller should run `feed(&[])` when this is true.
    pub fn has_staged(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Returns (non-mouse bytes, complete events).
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Vec<Event>) {
        if !self.active {
            let mut out = std::mem::take(&mut self.ready);
            out.append(&mut self.pending);
            self.pending_since = None;
            out.extend_from_slice(input);
            return (out, Vec::new());
        }
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(input);
        self.pending_since = None;
        let mut out = std::mem::take(&mut self.ready);
        let mut events = Vec::new();

        let mut i = 0;
        while i < data.len() {
            let rest = &data[i..];
            if rest.len() >= 3 && rest[0] == 0x1b && rest[1] == b'[' {
                let parsed = if rest[2] == b'M' {
                    // X10/UTF-8: ESC [ M cb cx cy (values may be UTF-8).
                    parse_x10(&data[i + 3..]).map(|(ev, used)| (ev, 3 + used))
                } else if rest[2] == b'<' {
                    // SGR: ESC [ < b ; x ; y (M press | m release)
                    parse_sgr(&data[i..])
                } else {
                    None
                };
                match parsed {
                    Some((ev, used)) => {
                        events.push(ev);
                        i += used;
                        continue;
                    }
                    None if rest[2] == b'M' || rest[2] == b'<' => {
                        // Report start without a complete tail: hold the
                        // remainder and stop scanning this chunk.
                        self.hold();
                        break;
                    }
                    _ => {}
                }
            }
            // Lone ESC or ESC[ at the chunk end is a prefix of a possible
            // report — hold briefly; Esc-as-key releases after HOLD.
            if rest[0] == 0x1b && is_mouse_prefix(rest) {
                self.hold();
                break;
            }
            out.push(rest[0]);
            i += 1;
        }
        self.pending.extend_from_slice(&data[i..]);
        if self.pending.len() > MAX_PENDING {
            // Malformed/stuck: release the oldest byte and keep scanning.
            out.extend_from_slice(&self.pending[..1]);
            self.pending.drain(..1);
        }
        (out, events)
    }

    /// Release held bytes once HOLD expires — the "report" never
    /// completed, so it was user input. The bytes are staged into `ready`
    /// and emitted by the next `feed`.
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

    fn hold(&mut self) {
        if self.pending_since.is_none() {
            self.pending_since = Some(Instant::now());
        }
    }
}

/// True when `rest` is a strict prefix of a mouse report start
/// (`ESC [ M` or `ESC [ <`) — i.e. a lone `ESC` or `ESC [` at the chunk end.
fn is_mouse_prefix(rest: &[u8]) -> bool {
    matches!(rest, [0x1b] | [0x1b, b'['])
}

/// Parse X10/UTF-8 event data `cb cx cy` (each value +32, possibly UTF-8
/// encoded for mode 1005). Returns (event, bytes consumed).
fn parse_x10(data: &[u8]) -> Option<(Event, usize)> {
    let (cb, n1) = read_cp(data, 0)?;
    let (cx, n2) = read_cp(data, n1)?;
    let (cy, n3) = read_cp(data, n1 + n2)?;
    let cb = cb.saturating_sub(32) as u8;
    Some((
        Event {
            cb,
            x: cx.saturating_sub(32) as u16,
            y: cy.saturating_sub(32) as u16,
            release: cb & 3 == 3,
        },
        n1 + n2 + n3,
    ))
}

/// Read one value: a byte, or a UTF-8 codepoint when >= 0x80 (mode 1005).
/// Returns (codepoint, bytes consumed) or None if incomplete.
fn read_cp(data: &[u8], at: usize) -> Option<(u32, usize)> {
    let b = *data.get(at)?;
    if b < 0x80 {
        return Some((b as u32, 1));
    }
    let len = if b >= 0xf0 {
        4
    } else if b >= 0xe0 {
        3
    } else {
        2
    };
    if at + len > data.len() {
        return None;
    }
    std::str::from_utf8(&data[at..at + len])
        .ok()
        .and_then(|s| s.chars().next())
        .map(|c| (c as u32, len))
}

/// Parse `ESC [ < b ; x ; y M/m`. Returns (event, bytes consumed).
fn parse_sgr(data: &[u8]) -> Option<(Event, usize)> {
    // Find the final byte.
    let mut i = 3;
    while i < data.len() {
        match data[i] {
            b'M' | b'm' => break,
            b'0'..=b'9' | b';' => i += 1,
            _ => return None, // not a SGR report — let caller pass it through
        }
    }
    if i >= data.len() {
        return None; // incomplete
    }
    let release = data[i] == b'm';
    let params = std::str::from_utf8(&data[3..i]).ok()?;
    let mut it = params.split(';');
    let cb: u8 = it.next()?.parse().ok()?;
    let x: u16 = it.next()?.parse().ok()?;
    let y: u16 = it.next()?.parse().ok()?;
    Some((
        Event {
            cb,
            x,
            y,
            release: release || cb & 3 == 3,
        },
        i + 1,
    ))
}

/// Encode an event for the child in the requested format:
/// 0 = X10, 5 = UTF-8 extended, 6 = SGR.
pub fn encode_report(fmt: u8, ev: &Event) -> Vec<u8> {
    match fmt {
        6 => {
            let fin = if ev.release { b'm' } else { b'M' };
            format!("\x1b[<{};{};{}{}", ev.cb, ev.x, ev.y, fin as char).into_bytes()
        }
        5 => {
            // UTF-8 extended: values +32 encoded as codepoints.
            let cb = if ev.release { (ev.cb & !3) | 3 } else { ev.cb };
            let mut out = b"\x1b[M".to_vec();
            for v in [cb as u32 + 32, ev.x as u32 + 32, ev.y as u32 + 32] {
                match char::from_u32(v) {
                    Some(c) => {
                        let mut b = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                    }
                    None => out.push(32),
                }
            }
            out
        }
        _ => {
            // Legacy X10: three bytes of value+32, clamped to 255.
            let cb = if ev.release { (ev.cb & !3) | 3 } else { ev.cb };
            let clamp = |v: u32| v.min(255) as u8;
            vec![
                0x1b,
                b'[',
                b'M',
                clamp(cb as u32 + 32),
                clamp(ev.x as u32 + 32),
                clamp(ev.y as u32 + 32),
            ]
        }
    }
}

/// DECSET string to enable mouse reporting on the outer terminal.
/// `tracking` is the mask to enable (bits 0-2 = 1000/1002/1003). A zero
/// mask returns an empty string: reporting stays off and the outer
/// terminal keeps its native wheel scrollback and drag selection.
/// SGR encoding (1006) is always requested so reports are unambiguous.
pub fn terminal_setup(tracking: u8) -> String {
    let mut s = String::new();
    if tracking & 1 != 0 {
        s.push_str("\x1b[?1000h");
    }
    if tracking & 2 != 0 {
        s.push_str("\x1b[?1002h");
    }
    if tracking & 4 != 0 {
        s.push_str("\x1b[?1003h");
    }
    if tracking != 0 {
        s.push_str("\x1b[?1006h");
    }
    s
}

/// DECRST string that disables every mode `terminal_setup` may enable.
/// Safe to emit unconditionally on cleanup.
pub fn terminal_teardown() -> &'static str {
    "\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[?1005l\x1b[?1007l"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_sgr_press_release() {
        let mut d = Decoder::new();
        d.set_active(true);
        let (kb, evs) = d.feed(b"\x1b[<0;10;5M\x1b[<0;10;5m");
        assert!(kb.is_empty());
        assert_eq!(
            evs,
            [
                Event {
                    cb: 0,
                    x: 10,
                    y: 5,
                    release: false
                },
                Event {
                    cb: 0,
                    x: 10,
                    y: 5,
                    release: true
                },
            ]
        );
    }

    #[test]
    fn decode_sgr_mixed_with_keys() {
        let mut d = Decoder::new();
        d.set_active(true);
        let (kb, evs) = d.feed(b"a\x1b[<64;3;2Mb");
        assert_eq!(kb, b"ab");
        assert_eq!(
            evs,
            [Event {
                cb: 64,
                x: 3,
                y: 2,
                release: false
            }]
        );
    }

    #[test]
    fn decode_x10() {
        let mut d = Decoder::new();
        d.set_active(true);
        // left press at col 10 row 5: cb=32(' '), cx=42('*'), cy=37('%')
        let (kb, evs) = d.feed(b"\x1b[M *%");
        assert!(kb.is_empty());
        assert_eq!(
            evs,
            [Event {
                cb: 0,
                x: 10,
                y: 5,
                release: false
            }]
        );
    }

    #[test]
    fn decode_x10_release() {
        let mut d = Decoder::new();
        d.set_active(true);
        let (kb, evs) = d.feed(b"\x1b[M#*5");
        assert!(kb.is_empty());
        assert!(evs[0].release);
    }

    #[test]
    fn split_report_held() {
        let mut d = Decoder::new();
        d.set_active(true);
        let (kb, evs) = d.feed(b"k\x1b[<0;1");
        assert_eq!(kb, b"k");
        assert!(evs.is_empty());
        let (kb, evs) = d.feed(b"0;5Mj");
        assert_eq!(kb, b"j");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].x, 10);
    }

    #[test]
    fn encode_sgr() {
        let ev = Event {
            cb: 0,
            x: 10,
            y: 5,
            release: false,
        };
        assert_eq!(encode_report(6, &ev), b"\x1b[<0;10;5M");
        let rel = Event {
            release: true,
            ..ev
        };
        assert_eq!(encode_report(6, &rel), b"\x1b[<0;10;5m");
    }

    #[test]
    fn encode_x10() {
        let ev = Event {
            cb: 0,
            x: 10,
            y: 5,
            release: false,
        };
        assert_eq!(encode_report(0, &ev), b"\x1b[M *%");
        let rel = Event {
            release: true,
            ..ev
        };
        assert_eq!(encode_report(0, &rel), b"\x1b[M#*%");
    }

    #[test]
    fn encode_x10_clamps() {
        let ev = Event {
            cb: 0,
            x: 300,
            y: 5,
            release: false,
        };
        let out = encode_report(0, &ev);
        assert_eq!(out[4], 255); // 300+32 clamped
    }

    #[test]
    fn encode_utf8_ext() {
        let ev = Event {
            cb: 0,
            x: 120,
            y: 40,
            release: false,
        };
        let out = encode_report(5, &ev);
        assert_eq!(&out[..3], b"\x1b[M");
        // cb: 32 → ' '; x: 152 → U+0098 (2 bytes); y: 72 → 'H'
        assert_eq!(out[3], b' ');
        assert!(out.len() == 3 + 1 + 2 + 1);
    }

    #[test]
    fn roundtrip_sgr() {
        let mut d = Decoder::new();
        d.set_active(true);
        let ev = Event {
            cb: 65, // wheel down
            x: 30,
            y: 7,
            release: false,
        };
        let bytes = encode_report(6, &ev);
        let (kb, evs) = d.feed(&bytes);
        assert!(kb.is_empty());
        assert_eq!(evs, [ev]);
    }

    #[test]
    fn inactive_decoder_passes_through() {
        // Reporting off: every byte — including a report-looking escape —
        // reaches the pane untouched and a bare Esc is never held.
        let mut d = Decoder::new();
        let (kb, evs) = d.feed(b"\x1b\x1b[<0;1;1Mabc");
        assert_eq!(kb, b"\x1b\x1b[<0;1;1Mabc");
        assert!(evs.is_empty());
    }

    #[test]
    fn deactivate_flushes_held_bytes() {
        let mut d = Decoder::new();
        d.set_active(true);
        let (kb, _) = d.feed(b"\x1b[<0;1");
        assert!(kb.is_empty()); // held as a possible report fragment
        d.set_active(false);
        let (kb, evs) = d.feed(b"x");
        assert_eq!(kb, b"\x1b[<0;1x");
        assert!(evs.is_empty());
    }

    #[test]
    fn setup_empty_without_tracking() {
        // No tracking mask → no DECSET at all: the outer terminal keeps
        // native wheel scrollback and drag selection.
        assert_eq!(terminal_setup(0), "");
        assert_eq!(terminal_setup(0b010), "\x1b[?1002h\x1b[?1006h");
        assert_eq!(terminal_setup(0b101), "\x1b[?1000h\x1b[?1003h\x1b[?1006h");
    }
}
