// Protocol: client↔server message protocol (encode/decode).
//
// Wire format: [4 bytes length (u32 LE)] [1 byte type] [payload]
// Length = 1 (type) + payload length.

use std::io::{self, Read, Write};

use crate::grid::{Attr, Cell, Color};

// ── Message types ───────────────────────────────────────────────────

/// Client → Server messages.
#[derive(Debug)]
pub enum ClientMsg {
    /// Initial handshake: client terminal size.
    Identify { rows: u16, cols: u16 },
    /// Raw keystrokes from the client's stdin → forward to PTY.
    PaneInput { data: Vec<u8> },
    /// Client terminal resized.
    Resize { rows: u16, cols: u16 },
    /// Client disconnecting.
    Detach,
    /// Create a new window.
    NewWindow,
    /// Switch to next window.
    NextWindow,
    /// Switch to previous window.
    PrevWindow,
    /// Select window by index.
    SelectWindow { index: u8 },
    /// Kill the active pane/window.
    KillPane,
    /// Create a new session and switch to it. Optional name.
    NewSession { name: Option<String> },
    /// Switch to next session.
    NextSession,
    /// Switch to previous session.
    PrevSession,
    /// Select a session by name.
    SelectSession { name: String },
    /// Kill the current session (and all its windows).
    KillSession,
    /// Request list of sessions on this server (for the selector).
    ListSessions,
    /// Kill the server entirely (used by `lrmux kill-server`).
    KillServer,
}

/// Server → Client messages.
#[derive(Debug)]
pub enum ServerMsg {
    /// Acknowledge identify, send initial grid dimensions.
    IdentifyAck { rows: u16, cols: u16 },
    /// Full grid snapshot (sent on first connect, window switch, or after resize).
    GridSnapshot {
        rows: u16,
        cols: u16,
        cells: Vec<Cell>,
        cursor_row: u16,
        cursor_col: u16,
        cursor_visible: bool,
    },
    /// Dirty rows update (sent after PTY output is parsed into the grid).
    GridUpdate {
        dirty: Vec<(u16, Vec<Cell>)>,
        cursor_row: u16,
        cursor_col: u16,
        cursor_visible: bool,
    },
    /// Scrollback rows that scrolled off the top since the last update.
    /// Sent before GridUpdate so the client can push them to scrollback.
    ScrollbackUpdate { rows: Vec<Vec<Cell>> },
    /// Child process exited.
    PaneExit { code: u8 },
    /// Error message.
    Error { msg: String },
    /// Status bar content (session name, window list, active window, session count).
    StatusBarUpdate {
        session: String,
        windows: Vec<String>,
        active: u16,
        session_count: u16,
    },
    /// List of session names on this server (response to ListSessions).
    SessionList { sessions: Vec<String> },
}

// ── Type tags ───────────────────────────────────────────────────────

const C_IDENTIFY: u8 = 0x01;
const C_PANE_INPUT: u8 = 0x02;
const C_RESIZE: u8 = 0x03;
const C_DETACH: u8 = 0x04;
const C_NEW_WINDOW: u8 = 0x05;
const C_NEXT_WINDOW: u8 = 0x06;
const C_PREV_WINDOW: u8 = 0x07;
const C_SELECT_WINDOW: u8 = 0x08;
const C_KILL_PANE: u8 = 0x09;
const C_NEW_SESSION: u8 = 0x0a;
const C_NEXT_SESSION: u8 = 0x0b;
const C_PREV_SESSION: u8 = 0x0c;
const C_SELECT_SESSION: u8 = 0x0f;
const C_KILL_SESSION: u8 = 0x0e;
const C_LIST_SESSIONS: u8 = 0x0d;
const C_KILL_SERVER: u8 = 0x10;

const S_IDENTIFY_ACK: u8 = 0x10;
const S_GRID_SNAPSHOT: u8 = 0x11;
const S_GRID_UPDATE: u8 = 0x12;
const S_SCROLLBACK_UPDATE: u8 = 0x17;
const S_PANE_EXIT: u8 = 0x13;
const S_ERROR: u8 = 0x14;
const S_STATUS_BAR: u8 = 0x15;
const S_SESSION_LIST: u8 = 0x16;

// ── Encode ──────────────────────────────────────────────────────────

/// Encode a client message into a byte buffer.
pub fn encode_client(msg: &ClientMsg) -> Vec<u8> {
    let mut payload = Vec::new();
    match msg {
        ClientMsg::Identify { rows, cols } => {
            payload.push(C_IDENTIFY);
            payload.extend_from_slice(&rows.to_le_bytes());
            payload.extend_from_slice(&cols.to_le_bytes());
        }
        ClientMsg::PaneInput { data } => {
            payload.push(C_PANE_INPUT);
            payload.extend_from_slice(data);
        }
        ClientMsg::Resize { rows, cols } => {
            payload.push(C_RESIZE);
            payload.extend_from_slice(&rows.to_le_bytes());
            payload.extend_from_slice(&cols.to_le_bytes());
        }
        ClientMsg::Detach => {
            payload.push(C_DETACH);
        }
        ClientMsg::NewWindow => {
            payload.push(C_NEW_WINDOW);
        }
        ClientMsg::NextWindow => {
            payload.push(C_NEXT_WINDOW);
        }
        ClientMsg::PrevWindow => {
            payload.push(C_PREV_WINDOW);
        }
        ClientMsg::SelectWindow { index } => {
            payload.push(C_SELECT_WINDOW);
            payload.push(*index);
        }
        ClientMsg::KillPane => {
            payload.push(C_KILL_PANE);
        }
        ClientMsg::NewSession { name } => {
            payload.push(C_NEW_SESSION);
            match name {
                Some(n) => {
                    payload.push(1);
                    payload.extend_from_slice(&(n.len() as u32).to_le_bytes());
                    payload.extend_from_slice(n.as_bytes());
                }
                None => payload.push(0),
            }
        }
        ClientMsg::NextSession => {
            payload.push(C_NEXT_SESSION);
        }
        ClientMsg::PrevSession => {
            payload.push(C_PREV_SESSION);
        }
        ClientMsg::SelectSession { name } => {
            payload.push(C_SELECT_SESSION);
            payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
            payload.extend_from_slice(name.as_bytes());
        }
        ClientMsg::KillSession => {
            payload.push(C_KILL_SESSION);
        }
        ClientMsg::ListSessions => {
            payload.push(C_LIST_SESSIONS);
        }
        ClientMsg::KillServer => {
            payload.push(C_KILL_SERVER);
        }
    }
    frame(payload)
}

/// Encode a server message into a byte buffer.
pub fn encode_server(msg: &ServerMsg) -> Vec<u8> {
    let mut payload = Vec::new();
    match msg {
        ServerMsg::IdentifyAck { rows, cols } => {
            payload.push(S_IDENTIFY_ACK);
            payload.extend_from_slice(&rows.to_le_bytes());
            payload.extend_from_slice(&cols.to_le_bytes());
        }
        ServerMsg::GridSnapshot {
            rows,
            cols,
            cells,
            cursor_row,
            cursor_col,
            cursor_visible,
        } => {
            payload.push(S_GRID_SNAPSHOT);
            payload.extend_from_slice(&rows.to_le_bytes());
            payload.extend_from_slice(&cols.to_le_bytes());
            payload.extend_from_slice(&(cells.len() as u32).to_le_bytes());
            for cell in cells {
                encode_cell(&mut payload, cell);
            }
            payload.extend_from_slice(&cursor_row.to_le_bytes());
            payload.extend_from_slice(&cursor_col.to_le_bytes());
            payload.push(*cursor_visible as u8);
        }
        ServerMsg::GridUpdate {
            dirty,
            cursor_row,
            cursor_col,
            cursor_visible,
        } => {
            payload.push(S_GRID_UPDATE);
            payload.extend_from_slice(&(dirty.len() as u32).to_le_bytes());
            for (row, cells) in dirty {
                payload.extend_from_slice(&row.to_le_bytes());
                payload.extend_from_slice(&(cells.len() as u32).to_le_bytes());
                for cell in cells {
                    encode_cell(&mut payload, cell);
                }
            }
            payload.extend_from_slice(&cursor_row.to_le_bytes());
            payload.extend_from_slice(&cursor_col.to_le_bytes());
            payload.push(*cursor_visible as u8);
        }
        ServerMsg::ScrollbackUpdate { rows } => {
            payload.push(S_SCROLLBACK_UPDATE);
            payload.extend_from_slice(&(rows.len() as u32).to_le_bytes());
            for row in rows {
                payload.extend_from_slice(&(row.len() as u32).to_le_bytes());
                for cell in row {
                    encode_cell(&mut payload, cell);
                }
            }
        }
        ServerMsg::PaneExit { code } => {
            payload.push(S_PANE_EXIT);
            payload.push(*code);
        }
        ServerMsg::Error { msg } => {
            payload.push(S_ERROR);
            payload.extend_from_slice(&(msg.len() as u32).to_le_bytes());
            payload.extend_from_slice(msg.as_bytes());
        }
        ServerMsg::StatusBarUpdate {
            session,
            windows,
            active,
            session_count,
        } => {
            payload.push(S_STATUS_BAR);
            payload.extend_from_slice(&(session.len() as u32).to_le_bytes());
            payload.extend_from_slice(session.as_bytes());
            payload.extend_from_slice(&(windows.len() as u32).to_le_bytes());
            for name in windows {
                payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
                payload.extend_from_slice(name.as_bytes());
            }
            payload.extend_from_slice(&active.to_le_bytes());
            payload.extend_from_slice(&session_count.to_le_bytes());
        }
        ServerMsg::SessionList { sessions } => {
            payload.push(S_SESSION_LIST);
            payload.extend_from_slice(&(sessions.len() as u32).to_le_bytes());
            for name in sessions {
                payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
                payload.extend_from_slice(name.as_bytes());
            }
        }
    }
    frame(payload)
}

/// Wrap payload with a 4-byte length header.
fn frame(payload: Vec<u8>) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = len.to_le_bytes().to_vec();
    out.extend_from_slice(&payload);
    out
}

/// Encode a single cell into a buffer.
fn encode_cell(buf: &mut Vec<u8>, cell: &Cell) {
    buf.extend_from_slice(&(cell.ch as u32).to_le_bytes());
    encode_color(buf, cell.fg);
    encode_color(buf, cell.bg);
    encode_attr(buf, cell.attrs);
}

fn encode_color(buf: &mut Vec<u8>, color: Color) {
    match color {
        Color::Default => buf.push(0),
        Color::Indexed(n) => {
            buf.push(1);
            buf.push(n);
        }
        Color::Rgb(r, g, b) => {
            buf.push(2);
            buf.push(r);
            buf.push(g);
            buf.push(b);
        }
    }
}

fn encode_attr(buf: &mut Vec<u8>, attrs: Attr) {
    let mut bits = 0u8;
    if attrs.bold {
        bits |= 0x01;
    }
    if attrs.italic {
        bits |= 0x02;
    }
    if attrs.underline {
        bits |= 0x04;
    }
    if attrs.reverse {
        bits |= 0x08;
    }
    buf.push(bits);
}

// ── Decode ──────────────────────────────────────────────────────────

/// Read one framed message from a stream. Returns the type tag + payload.
fn read_frame<R: Read>(reader: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zero-length frame",
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    let msg_type = payload[0];
    Ok((msg_type, payload[1..].to_vec()))
}

/// Read and decode a client message from a stream.
pub fn decode_client<R: Read>(reader: &mut R) -> io::Result<ClientMsg> {
    let (tag, data) = read_frame(reader)?;
    let mut r = &data[..];
    match tag {
        C_IDENTIFY => {
            let rows = read_u16(&mut r)?;
            let cols = read_u16(&mut r)?;
            Ok(ClientMsg::Identify { rows, cols })
        }
        C_PANE_INPUT => Ok(ClientMsg::PaneInput { data: r.to_vec() }),
        C_RESIZE => {
            let rows = read_u16(&mut r)?;
            let cols = read_u16(&mut r)?;
            Ok(ClientMsg::Resize { rows, cols })
        }
        C_DETACH => Ok(ClientMsg::Detach),
        C_NEW_WINDOW => Ok(ClientMsg::NewWindow),
        C_NEXT_WINDOW => Ok(ClientMsg::NextWindow),
        C_PREV_WINDOW => Ok(ClientMsg::PrevWindow),
        C_SELECT_WINDOW => {
            let index = read_u8(&mut r)?;
            Ok(ClientMsg::SelectWindow { index })
        }
        C_KILL_PANE => Ok(ClientMsg::KillPane),
        C_NEW_SESSION => {
            let has_name = read_u8(&mut r)?;
            if has_name != 0 {
                let len = read_u32(&mut r)? as usize;
                let name = String::from_utf8_lossy(&r[..len]).into_owned();
                Ok(ClientMsg::NewSession { name: Some(name) })
            } else {
                Ok(ClientMsg::NewSession { name: None })
            }
        }
        C_NEXT_SESSION => Ok(ClientMsg::NextSession),
        C_PREV_SESSION => Ok(ClientMsg::PrevSession),
        C_SELECT_SESSION => {
            let len = read_u32(&mut r)? as usize;
            let name = String::from_utf8_lossy(&r[..len]).into_owned();
            Ok(ClientMsg::SelectSession { name })
        }
        C_KILL_SESSION => Ok(ClientMsg::KillSession),
        C_LIST_SESSIONS => Ok(ClientMsg::ListSessions),
        C_KILL_SERVER => Ok(ClientMsg::KillServer),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown client msg type: {tag}"),
        )),
    }
}

/// Read and decode a server message from a stream.
pub fn decode_server<R: Read>(reader: &mut R) -> io::Result<ServerMsg> {
    let (tag, data) = read_frame(reader)?;
    let mut r = &data[..];
    match tag {
        S_IDENTIFY_ACK => {
            let rows = read_u16(&mut r)?;
            let cols = read_u16(&mut r)?;
            Ok(ServerMsg::IdentifyAck { rows, cols })
        }
        S_GRID_SNAPSHOT => {
            let rows = read_u16(&mut r)?;
            let cols = read_u16(&mut r)?;
            let count = read_u32(&mut r)? as usize;
            let mut cells = Vec::with_capacity(count);
            for _ in 0..count {
                cells.push(decode_cell(&mut r)?);
            }
            let cursor_row = read_u16(&mut r)?;
            let cursor_col = read_u16(&mut r)?;
            let cursor_visible = read_u8(&mut r)? != 0;
            Ok(ServerMsg::GridSnapshot {
                rows,
                cols,
                cells,
                cursor_row,
                cursor_col,
                cursor_visible,
            })
        }
        S_GRID_UPDATE => {
            let dirty_count = read_u32(&mut r)? as usize;
            let mut dirty = Vec::with_capacity(dirty_count);
            for _ in 0..dirty_count {
                let row = read_u16(&mut r)?;
                let cell_count = read_u32(&mut r)? as usize;
                let mut cells = Vec::with_capacity(cell_count);
                for _ in 0..cell_count {
                    cells.push(decode_cell(&mut r)?);
                }
                dirty.push((row, cells));
            }
            let cursor_row = read_u16(&mut r)?;
            let cursor_col = read_u16(&mut r)?;
            let cursor_visible = read_u8(&mut r)? != 0;
            Ok(ServerMsg::GridUpdate {
                dirty,
                cursor_row,
                cursor_col,
                cursor_visible,
            })
        }
        S_SCROLLBACK_UPDATE => {
            let row_count = read_u32(&mut r)? as usize;
            let mut rows = Vec::with_capacity(row_count);
            for _ in 0..row_count {
                let cell_count = read_u32(&mut r)? as usize;
                let mut row = Vec::with_capacity(cell_count);
                for _ in 0..cell_count {
                    row.push(decode_cell(&mut r)?);
                }
                rows.push(row);
            }
            Ok(ServerMsg::ScrollbackUpdate { rows })
        }
        S_PANE_EXIT => {
            let code = read_u8(&mut r)?;
            Ok(ServerMsg::PaneExit { code })
        }
        S_ERROR => {
            let len = read_u32(&mut r)? as usize;
            let bytes = r[..len].to_vec();
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            Ok(ServerMsg::Error { msg })
        }
        S_STATUS_BAR => {
            let session_len = read_u32(&mut r)? as usize;
            let session = String::from_utf8_lossy(&r[..session_len]).into_owned();
            r = &r[session_len..];
            let count = read_u32(&mut r)? as usize;
            let mut windows = Vec::with_capacity(count);
            for _ in 0..count {
                let len = read_u32(&mut r)? as usize;
                let bytes = r[..len].to_vec();
                r = &r[len..];
                windows.push(String::from_utf8_lossy(&bytes).into_owned());
            }
            let active = read_u16(&mut r)?;
            let session_count = read_u16(&mut r)?;
            Ok(ServerMsg::StatusBarUpdate {
                session,
                windows,
                active,
                session_count,
            })
        }
        S_SESSION_LIST => {
            let count = read_u32(&mut r)? as usize;
            let mut sessions = Vec::with_capacity(count);
            for _ in 0..count {
                let len = read_u32(&mut r)? as usize;
                let bytes = r[..len].to_vec();
                r = &r[len..];
                sessions.push(String::from_utf8_lossy(&bytes).into_owned());
            }
            Ok(ServerMsg::SessionList { sessions })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown server msg type: {tag}"),
        )),
    }
}

fn decode_cell(r: &mut &[u8]) -> io::Result<Cell> {
    let ch_u = read_u32(r)?;
    let ch = char::from_u32(ch_u).unwrap_or('\0');
    let fg = decode_color(r)?;
    let bg = decode_color(r)?;
    let attrs = decode_attr(r)?;
    Ok(Cell { ch, fg, bg, attrs })
}

fn decode_color(r: &mut &[u8]) -> io::Result<Color> {
    let tag = read_u8(r)?;
    match tag {
        0 => Ok(Color::Default),
        1 => Ok(Color::Indexed(read_u8(r)?)),
        2 => {
            let red = read_u8(r)?;
            let green = read_u8(r)?;
            let blue = read_u8(r)?;
            Ok(Color::Rgb(red, green, blue))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown color tag: {tag}"),
        )),
    }
}

fn decode_attr(r: &mut &[u8]) -> io::Result<Attr> {
    let bits = read_u8(r)?;
    Ok(Attr {
        bold: bits & 0x01 != 0,
        italic: bits & 0x02 != 0,
        underline: bits & 0x04 != 0,
        reverse: bits & 0x08 != 0,
    })
}

// ── Helpers ─────────────────────────────────────────────────────────

fn read_u8(r: &mut &[u8]) -> io::Result<u8> {
    if r.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "expected 1 byte",
        ));
    }
    let val = r[0];
    *r = &r[1..];
    Ok(val)
}

fn read_u16(r: &mut &[u8]) -> io::Result<u16> {
    if r.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "expected 2 bytes",
        ));
    }
    let val = u16::from_le_bytes([r[0], r[1]]);
    *r = &r[2..];
    Ok(val)
}

fn read_u32(r: &mut &[u8]) -> io::Result<u32> {
    if r.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "expected 4 bytes",
        ));
    }
    let val = u32::from_le_bytes([r[0], r[1], r[2], r[3]]);
    *r = &r[4..];
    Ok(val)
}

/// Write a framed message to a stream.
pub fn send<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}
