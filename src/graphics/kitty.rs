//! Parse Kitty graphics APC `_G key=val,…;payload` chunks.

use std::collections::HashMap;

/// Result of applying one or more completed Kitty commands.
#[derive(Debug, Clone)]
pub enum GraphicsEvent {
    /// Image bytes ready to store / ship to clients.
    Transmit {
        image_id: u32,
        width: u32,
        height: u32,
        format: u8,
        zlib: bool,
        data: Vec<u8>,
        /// Virtual placement size when `U=1` (or normalized).
        place_cols: Option<u32>,
        place_rows: Option<u32>,
        /// True when the original command also displayed at the cursor
        /// without U=1 — caller should synthesize placeholders.
        need_placeholders: bool,
    },
    Delete {
        image_id: u32,
    },
    /// Quiet query reply to write back to the PTY.
    QueryReply {
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsAction {
    Transmit,
    TransmitAndDisplay,
    Put,
    Delete,
    Query,
    Other,
}

#[derive(Default)]
struct ChunkState {
    keys: HashMap<String, String>,
    payload: Vec<u8>,
}

/// Accumulates chunked Kitty APC frames until `m=0`.
#[derive(Default)]
pub struct KittyParser {
    pending: Option<ChunkState>,
}

impl KittyParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one complete APC body after `_G` (keys;payload), without the
    /// surrounding ESC/_/ST. Returns zero or more completed events.
    pub fn feed_apc_body(&mut self, body: &[u8]) -> Vec<GraphicsEvent> {
        let Some((keys_raw, payload_b64)) = split_keys_payload(body) else {
            return Vec::new();
        };
        let keys = parse_keys(keys_raw);
        let more = keys.get("m").map(|s| s == "1").unwrap_or(false);
        let chunk = base64_decode(payload_b64);

        if let Some(ref mut st) = self.pending {
            st.payload.extend_from_slice(&chunk);
            if more {
                return Vec::new();
            }
            let done = self.pending.take().unwrap();
            return finalize(done.keys, done.payload);
        }

        if more {
            self.pending = Some(ChunkState {
                keys,
                payload: chunk,
            });
            return Vec::new();
        }

        finalize(keys, chunk)
    }
}

/// Parse a single (possibly complete) `_G…` APC body into events.
pub fn parse_apc_g(body: &[u8]) -> Vec<GraphicsEvent> {
    let mut p = KittyParser::new();
    p.feed_apc_body(body)
}

fn finalize(keys: HashMap<String, String>, payload: Vec<u8>) -> Vec<GraphicsEvent> {
    let action = match keys.get("a").map(|s| s.as_str()).unwrap_or("t") {
        "t" => GraphicsAction::Transmit,
        "T" => GraphicsAction::TransmitAndDisplay,
        "p" => GraphicsAction::Put,
        "d" => GraphicsAction::Delete,
        "q" => GraphicsAction::Query,
        _ => GraphicsAction::Other,
    };
    let image_id = keys.get("i").and_then(|s| s.parse().ok()).unwrap_or(0);
    match action {
        GraphicsAction::Query => {
            let reply = format!("\x1b_Gi={image_id};OK\x1b\\").into_bytes();
            vec![GraphicsEvent::QueryReply { bytes: reply }]
        }
        GraphicsAction::Delete => {
            vec![GraphicsEvent::Delete { image_id }]
        }
        GraphicsAction::Put => {
            let cols = keys.get("c").and_then(|s| s.parse().ok());
            let rows = keys.get("r").and_then(|s| s.parse().ok());
            let virtual_u = keys.get("U").map(|s| s == "1").unwrap_or(false);
            if virtual_u {
                // Placement only — image data already transmitted.
                vec![GraphicsEvent::Transmit {
                    image_id,
                    width: 0,
                    height: 0,
                    format: 32,
                    zlib: false,
                    data: Vec::new(),
                    place_cols: cols,
                    place_rows: rows,
                    need_placeholders: cols.is_some() && rows.is_some(),
                }]
            } else {
                Vec::new()
            }
        }
        GraphicsAction::Transmit | GraphicsAction::TransmitAndDisplay => {
            let width = keys.get("s").and_then(|s| s.parse().ok()).unwrap_or(0);
            let height = keys.get("v").and_then(|s| s.parse().ok()).unwrap_or(0);
            let format = keys.get("f").and_then(|s| s.parse().ok()).unwrap_or(32);
            let zlib = keys.get("o").map(|s| s == "z").unwrap_or(false);
            let virtual_u = keys.get("U").map(|s| s == "1").unwrap_or(false);
            let cols = keys.get("c").and_then(|s| s.parse().ok());
            let rows = keys.get("r").and_then(|s| s.parse().ok());
            let need_placeholders = match action {
                GraphicsAction::TransmitAndDisplay if virtual_u => cols.is_some() && rows.is_some(),
                GraphicsAction::TransmitAndDisplay => {
                    // Cursor placement — normalize to placeholders using
                    // c/r when present; otherwise caller may skip.
                    true
                }
                _ => virtual_u && cols.is_some() && rows.is_some(),
            };
            vec![GraphicsEvent::Transmit {
                image_id,
                width,
                height,
                format,
                zlib,
                data: payload,
                place_cols: cols,
                place_rows: rows,
                need_placeholders,
            }]
        }
        GraphicsAction::Other => Vec::new(),
    }
}

fn split_keys_payload(body: &[u8]) -> Option<(&[u8], &[u8])> {
    match body.iter().position(|&b| b == b';') {
        Some(semi) => Some((&body[..semi], &body[semi + 1..])),
        // Delete / place commands often omit the payload separator.
        None => Some((body, b"")),
    }
}

fn parse_keys(raw: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in raw.split(|&b| b == b',') {
        if part.is_empty() {
            continue;
        }
        if let Some(eq) = part.iter().position(|&b| b == b'=') {
            let k = String::from_utf8_lossy(&part[..eq]).into_owned();
            let v = String::from_utf8_lossy(&part[eq + 1..]).into_owned();
            map.insert(k, v);
        }
    }
    map
}

fn base64_decode(input: &[u8]) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let filtered: Vec<u8> = input
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(filtered.len() * 3 / 4);
    let mut i = 0;
    while i + 4 <= filtered.len() {
        let (a, b, c, d) = (
            val(filtered[i]).unwrap_or(0) as u32,
            val(filtered[i + 1]).unwrap_or(0) as u32,
            val(filtered[i + 2]).unwrap_or(0) as u32,
            val(filtered[i + 3]).unwrap_or(0) as u32,
        );
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
        i += 4;
    }
    let rem = filtered.len() - i;
    if rem >= 2 {
        let a = val(filtered[i]).unwrap_or(0) as u32;
        let b = val(filtered[i + 1]).unwrap_or(0) as u32;
        let c = if rem >= 3 {
            val(filtered[i + 2]).unwrap_or(0) as u32
        } else {
            0
        };
        let n = (a << 18) | (b << 12) | (c << 6);
        out.push((n >> 16) as u8);
        if rem >= 3 {
            out.push((n >> 8) as u8);
        }
    }
    out
}

/// Scan `input` for complete APC `_G…ST` and DCS `tmux;…ST` sequences.
/// Returns (events from completed APCs, bytes with those sequences removed
/// for the VT parser, leftover incomplete prefix to keep).
pub fn extract_graphics(input: &[u8]) -> (Vec<GraphicsEvent>, Vec<u8>, Vec<u8>) {
    let mut events = Vec::new();
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    let mut parser = KittyParser::new();
    while i < input.len() {
        if input[i] != 0x1b {
            out.push(input[i]);
            i += 1;
            continue;
        }
        if i + 1 >= input.len() {
            // Lone ESC — keep for next read.
            return (events, out, input[i..].to_vec());
        }
        let kind = input[i + 1];
        if kind == b'_' {
            // APC: ESC _ … ST (ESC \) or BEL
            if let Some((end, body)) = take_string_body(&input[i + 2..]) {
                let abs_end = i + 2 + end;
                if body.first() == Some(&b'G') {
                    events.extend(parser.feed_apc_body(&body[1..]));
                }
                i = abs_end;
                continue;
            }
            return (events, out, input[i..].to_vec());
        }
        if kind == b'P' {
            // DCS: ESC P … ST. tmux passthrough doubles ESC inside the
            // body, so a naive ST search would stop at the inner `ESC ESC \`
            // (doubled form of the wrapped sequence's own ST).
            if let Some((end, body)) = take_dcs_body(&input[i + 2..]) {
                let abs_end = i + 2 + end;
                if let Some(inner) = unwrap_tmux_dcs(body) {
                    let (ev, cleaned, _left) = extract_graphics(&inner);
                    events.extend(ev);
                    out.extend_from_slice(&cleaned);
                }
                // Drop the DCS wrapper either way (passthrough consumed).
                i = abs_end;
                continue;
            }
            return (events, out, input[i..].to_vec());
        }
        out.push(input[i]);
        i += 1;
    }
    (events, out, Vec::new())
}

fn take_string_body(rest: &[u8]) -> Option<(usize, &[u8])> {
    // Find ST = ESC \ or BEL.
    let mut j = 0;
    while j < rest.len() {
        if rest[j] == 0x07 {
            return Some((j + 1, &rest[..j]));
        }
        if rest[j] == 0x1b && j + 1 < rest.len() && rest[j + 1] == b'\\' {
            return Some((j + 2, &rest[..j]));
        }
        j += 1;
    }
    None
}

/// DCS body terminator that respects tmux's doubled-ESC escaping.
fn take_dcs_body(rest: &[u8]) -> Option<(usize, &[u8])> {
    let tmux = rest.starts_with(b"tmux;");
    let mut j = 0;
    while j < rest.len() {
        if rest[j] == 0x07 {
            return Some((j + 1, &rest[..j]));
        }
        if rest[j] == 0x1b && j + 1 < rest.len() {
            if tmux && rest[j + 1] == 0x1b {
                // Escaped ESC inside tmux passthrough — skip both.
                j += 2;
                continue;
            }
            if rest[j + 1] == b'\\' {
                return Some((j + 2, &rest[..j]));
            }
        }
        j += 1;
    }
    None
}

/// Unwrap `tmux;<bytes with doubled ESC>` into the inner byte string.
fn unwrap_tmux_dcs(body: &[u8]) -> Option<Vec<u8>> {
    const PREFIX: &[u8] = b"tmux;";
    if !body.starts_with(PREFIX) {
        return None;
    }
    let mut out = Vec::with_capacity(body.len());
    let mut i = PREFIX.len();
    while i < body.len() {
        if body[i] == 0x1b && i + 1 < body.len() && body[i + 1] == 0x1b {
            out.push(0x1b);
            i += 2;
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_chunk_transmit() {
        let body = b"a=T,f=32,o=z,s=1,v=1,t=d,i=7,U=1,c=2,r=2,q=2,m=0;AAAA";
        let ev = parse_apc_g(body);
        assert_eq!(ev.len(), 1);
        match &ev[0] {
            GraphicsEvent::Transmit {
                image_id,
                place_cols,
                place_rows,
                need_placeholders,
                ..
            } => {
                assert_eq!(*image_id, 7);
                assert_eq!(*place_cols, Some(2));
                assert_eq!(*place_rows, Some(2));
                assert!(*need_placeholders);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn extract_apc_from_stream() {
        let seq = b"hello\x1b_Ga=q,i=1,t=d,f=24,s=1,v=1;AAAA\x1b\\world";
        let (ev, cleaned, left) = extract_graphics(seq);
        assert!(left.is_empty());
        assert_eq!(cleaned, b"helloworld");
        assert!(matches!(ev[0], GraphicsEvent::QueryReply { .. }));
    }

    #[test]
    fn extract_tmux_wrapped_apc() {
        // ESC P tmux; ESC ESC _ G … ESC ESC \ ESC \
        let mut seq = Vec::new();
        seq.extend_from_slice(b"\x1bPtmux;");
        for &b in b"\x1b_Ga=d,d=I,i=5,q=2\x1b\\" {
            if b == 0x1b {
                seq.push(0x1b);
            }
            seq.push(b);
        }
        seq.extend_from_slice(b"\x1b\\");
        let (ev, _cleaned, left) = extract_graphics(&seq);
        assert!(left.is_empty());
        assert!(matches!(ev[0], GraphicsEvent::Delete { image_id: 5 }));
    }
}
