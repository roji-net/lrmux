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
    /// `attach: false` for CLI commands (no snapshot needed).
    /// `auth_token` is required on TCP when the server has one configured;
    /// ignored for Unix-socket clients.
    Identify {
        rows: u16,
        cols: u16,
        attach: bool,
        auth_token: String,
    },
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
    /// Create a new session and switch to it. Optional name, optional CWD.
    NewSession {
        name: Option<String>,
        cwd: Option<String>,
        command: Option<String>,
    },
    /// Switch to next session.
    NextSession,
    /// Switch to previous session.
    PrevSession,
    /// Select a session by name.
    SelectSession { name: String },
    /// Kill the current session (and all its windows).
    KillSession,
    /// Rename the current session.
    RenameSession { name: String },
    /// Request list of sessions on this server (for the selector).
    ListSessions,
    /// Kill the server entirely (used by `lrmux kill-server`).
    KillServer,
    /// Create a new window in a specific session (CLI: new-window).
    /// If session is None, uses the first session.
    /// If command is None, spawns the default shell.
    NewWindowIn {
        session: Option<String>,
        command: Option<String>,
    },
    /// Capture the content of a specific window (CLI: capture-window).
    /// If session is None, uses the first session.
    /// If window is None, uses the active window.
    /// `format`: 0=ascii, 1=ansi, 2=html, 3=markdown.
    /// `colors`: include fg/bg/attrs when the format supports them.
    /// Optional `term_fg` / `term_bg` override the pane's cached defaults.
    /// Prefer leaving these unset so HTML uses the captured pane's palette.
    CaptureWindow {
        session: Option<String>,
        window: Option<u8>,
        format: u8,
        colors: bool,
        term_fg: Option<(u8, u8, u8)>,
        term_bg: Option<(u8, u8, u8)>,
    },
    /// Send keys to a specific window's PTY (CLI: send-keys).
    /// If session is None, uses the first session.
    /// If window is None, uses the active window.
    SendKeys {
        session: Option<String>,
        window: Option<u8>,
        keys: Vec<u8>,
    },
    /// Request the server's in-memory ring log.
    GetLog,
    /// Control mode client identifies itself.
    /// The server will send text notifications instead of grid updates.
    IdentifyControl {
        rows: u16,
        cols: u16,
        auth_token: String,
    },
    /// Control mode client sends a tmux-style command line.
    /// The server parses it using the cmd module and executes it.
    ControlCommand { line: String },
    /// Request a fresh grid snapshot from the server.
    Refresh,
    /// Reply to a proxied OSC 10/11 color query (bytes from the real TTY).
    TermOscReply { pane_id: u32, data: Vec<u8> },
    /// Attached client's own default fg/bg (probed via OSC 10/11 on attach).
    /// Used so HTML capture has a page background even before vim asks.
    TermPalette {
        fg: Option<(u8, u8, u8)>,
        bg: Option<(u8, u8, u8)>,
    },
    /// Set / replace the server PSK (hot-apply + persist). Scaffolding for
    /// in-session network setup; fullscreen config editor comes later.
    SetPsk { psk: String },
}

/// Server → Client messages.
#[derive(Debug)]
pub enum ServerMsg {
    /// Acknowledge identify, send initial grid dimensions, version, and address.
    IdentifyAck {
        rows: u16,
        cols: u16,
        version: String,
        address: String,
    },
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
    ScrollbackUpdate {
        rows: Vec<Vec<Cell>>,
        /// True when this is a history replay after a GridSnapshot (window
        /// switch / attach); false for live scroll lines. The client uses
        /// it to decide whether to scroll the host terminal.
        replay: bool,
    },
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
        high_output: bool,
        /// Server name. Empty on messages from an older server.
        server: String,
    },
    /// List of session names + server address (response to ListSessions).
    SessionList {
        sessions: Vec<String>,
        address: String,
    },
    /// Captured window content (response to CaptureWindow).
    WindowCapture { content: String },
    /// Server ring log (response to GetLog).
    LogContent { lines: Vec<String> },
    /// Control mode notification: a text line to print to the control client's stdout.
    /// Format: "%window-add @1", "%output %0 hello", "%session-changed $1 name", etc.
    ControlNotify { line: String },
    /// Ask the client to query its real TTY for OSC 10/11 and reply with
    /// `TermOscReply` (so we don't invent palette colors).
    TermOscQuery {
        pane_id: u32,
        code: u8,
        bell_terminated: bool,
    },
    /// Acknowledge SetPsk (PSK was applied on the server).
    PskUpdated,
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
const C_RENAME_SESSION: u8 = 0x11;
const C_NEW_WINDOW_IN: u8 = 0x12;
const C_CAPTURE_WINDOW: u8 = 0x13;
const C_SEND_KEYS: u8 = 0x14;
const C_GET_LOG: u8 = 0x15;
const C_IDENTIFY_CONTROL: u8 = 0x16;
const C_CONTROL_COMMAND: u8 = 0x17;
const C_REFRESH: u8 = 0x18;
const C_TERM_OSC_REPLY: u8 = 0x19;
const C_TERM_PALETTE: u8 = 0x1a;
const C_SET_PSK: u8 = 0x1b;

const S_IDENTIFY_ACK: u8 = 0x10;
const S_GRID_SNAPSHOT: u8 = 0x11;
const S_GRID_UPDATE: u8 = 0x12;
const S_SCROLLBACK_UPDATE: u8 = 0x17;
const S_PANE_EXIT: u8 = 0x13;
const S_ERROR: u8 = 0x14;
const S_STATUS_BAR: u8 = 0x15;
const S_SESSION_LIST: u8 = 0x16;
const S_WINDOW_CAPTURE: u8 = 0x18;
const S_LOG_CONTENT: u8 = 0x19;
const S_CONTROL_NOTIFY: u8 = 0x1a;
const S_TERM_OSC_QUERY: u8 = 0x1b;
const S_PSK_UPDATED: u8 = 0x1c;

// ── Encode ──────────────────────────────────────────────────────────

/// Encode a client message into a byte buffer.
pub fn encode_client(msg: &ClientMsg) -> Vec<u8> {
    let mut payload = Vec::new();
    match msg {
        ClientMsg::Identify {
            rows,
            cols,
            attach,
            auth_token,
        } => {
            payload.push(C_IDENTIFY);
            payload.extend_from_slice(&rows.to_le_bytes());
            payload.extend_from_slice(&cols.to_le_bytes());
            payload.push(if *attach { 1 } else { 0 });
            payload.extend_from_slice(&(auth_token.len() as u32).to_le_bytes());
            payload.extend_from_slice(auth_token.as_bytes());
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
        ClientMsg::NewSession { name, cwd, command } => {
            payload.push(C_NEW_SESSION);
            match name {
                Some(n) => {
                    payload.push(1);
                    payload.extend_from_slice(&(n.len() as u32).to_le_bytes());
                    payload.extend_from_slice(n.as_bytes());
                }
                None => payload.push(0),
            }
            match cwd {
                Some(c) => {
                    payload.push(1);
                    payload.extend_from_slice(&(c.len() as u32).to_le_bytes());
                    payload.extend_from_slice(c.as_bytes());
                }
                None => payload.push(0),
            }
            match command {
                Some(c) => {
                    payload.push(1);
                    payload.extend_from_slice(&(c.len() as u32).to_le_bytes());
                    payload.extend_from_slice(c.as_bytes());
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
        ClientMsg::RenameSession { name } => {
            payload.push(C_RENAME_SESSION);
            payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
            payload.extend_from_slice(name.as_bytes());
        }
        ClientMsg::ListSessions => {
            payload.push(C_LIST_SESSIONS);
        }
        ClientMsg::KillServer => {
            payload.push(C_KILL_SERVER);
        }
        ClientMsg::NewWindowIn { session, command } => {
            payload.push(C_NEW_WINDOW_IN);
            match session {
                Some(s) => {
                    payload.push(1);
                    payload.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    payload.extend_from_slice(s.as_bytes());
                }
                None => payload.push(0),
            }
            match command {
                Some(c) => {
                    payload.push(1);
                    payload.extend_from_slice(&(c.len() as u32).to_le_bytes());
                    payload.extend_from_slice(c.as_bytes());
                }
                None => payload.push(0),
            }
        }
        ClientMsg::CaptureWindow {
            session,
            window,
            format,
            colors,
            term_fg,
            term_bg,
        } => {
            payload.push(C_CAPTURE_WINDOW);
            match session {
                Some(s) => {
                    payload.push(1);
                    payload.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    payload.extend_from_slice(s.as_bytes());
                }
                None => payload.push(0),
            }
            match window {
                Some(w) => {
                    payload.push(1);
                    payload.push(*w);
                }
                None => payload.push(0),
            }
            payload.push(*format);
            payload.push(if *colors { 1 } else { 0 });
            // Optional outer-TTY palette (older peers ignore/omit trailing bytes).
            if term_fg.is_none() && term_bg.is_none() {
                payload.push(0);
            } else {
                payload.push(1);
                match term_fg {
                    Some((r, g, b)) => {
                        payload.push(1);
                        payload.extend_from_slice(&[*r, *g, *b]);
                    }
                    None => payload.push(0),
                }
                match term_bg {
                    Some((r, g, b)) => {
                        payload.push(1);
                        payload.extend_from_slice(&[*r, *g, *b]);
                    }
                    None => payload.push(0),
                }
            }
        }
        ClientMsg::SendKeys {
            session,
            window,
            keys,
        } => {
            payload.push(C_SEND_KEYS);
            match session {
                Some(s) => {
                    payload.push(1);
                    payload.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    payload.extend_from_slice(s.as_bytes());
                }
                None => payload.push(0),
            }
            match window {
                Some(w) => {
                    payload.push(1);
                    payload.push(*w);
                }
                None => payload.push(0),
            }
            payload.extend_from_slice(&(keys.len() as u32).to_le_bytes());
            payload.extend_from_slice(keys);
        }
        ClientMsg::GetLog => {
            payload.push(C_GET_LOG);
        }
        ClientMsg::IdentifyControl {
            rows,
            cols,
            auth_token,
        } => {
            payload.push(C_IDENTIFY_CONTROL);
            payload.extend_from_slice(&rows.to_le_bytes());
            payload.extend_from_slice(&cols.to_le_bytes());
            payload.extend_from_slice(&(auth_token.len() as u32).to_le_bytes());
            payload.extend_from_slice(auth_token.as_bytes());
        }
        ClientMsg::ControlCommand { line } => {
            payload.push(C_CONTROL_COMMAND);
            payload.extend_from_slice(&(line.len() as u32).to_le_bytes());
            payload.extend_from_slice(line.as_bytes());
        }
        ClientMsg::Refresh => {
            payload.push(C_REFRESH);
        }
        ClientMsg::TermOscReply { pane_id, data } => {
            payload.push(C_TERM_OSC_REPLY);
            payload.extend_from_slice(&pane_id.to_le_bytes());
            payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
            payload.extend_from_slice(data);
        }
        ClientMsg::TermPalette { fg, bg } => {
            payload.push(C_TERM_PALETTE);
            match fg {
                Some((r, g, b)) => {
                    payload.push(1);
                    payload.extend_from_slice(&[*r, *g, *b]);
                }
                None => payload.push(0),
            }
            match bg {
                Some((r, g, b)) => {
                    payload.push(1);
                    payload.extend_from_slice(&[*r, *g, *b]);
                }
                None => payload.push(0),
            }
        }
        ClientMsg::SetPsk { psk } => {
            payload.push(C_SET_PSK);
            payload.extend_from_slice(&(psk.len() as u32).to_le_bytes());
            payload.extend_from_slice(psk.as_bytes());
        }
    }
    frame(payload)
}

/// Encode a server message into a byte buffer.
pub fn encode_server(msg: &ServerMsg) -> Vec<u8> {
    let mut payload = Vec::new();
    match msg {
        ServerMsg::IdentifyAck {
            rows,
            cols,
            version,
            address,
        } => {
            payload.push(S_IDENTIFY_ACK);
            payload.extend_from_slice(&rows.to_le_bytes());
            payload.extend_from_slice(&cols.to_le_bytes());
            payload.extend_from_slice(&(version.len() as u32).to_le_bytes());
            payload.extend_from_slice(version.as_bytes());
            payload.extend_from_slice(&(address.len() as u32).to_le_bytes());
            payload.extend_from_slice(address.as_bytes());
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
        ServerMsg::ScrollbackUpdate { rows, replay } => {
            payload.push(S_SCROLLBACK_UPDATE);
            payload.push(*replay as u8);
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
            high_output,
            server,
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
            payload.push(*high_output as u8);
            payload.extend_from_slice(&(server.len() as u32).to_le_bytes());
            payload.extend_from_slice(server.as_bytes());
        }
        ServerMsg::SessionList { sessions, address } => {
            payload.push(S_SESSION_LIST);
            payload.extend_from_slice(&(sessions.len() as u32).to_le_bytes());
            for name in sessions {
                payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
                payload.extend_from_slice(name.as_bytes());
            }
            payload.extend_from_slice(&(address.len() as u32).to_le_bytes());
            payload.extend_from_slice(address.as_bytes());
        }
        ServerMsg::WindowCapture { content } => {
            payload.push(S_WINDOW_CAPTURE);
            payload.extend_from_slice(&(content.len() as u32).to_le_bytes());
            payload.extend_from_slice(content.as_bytes());
        }
        ServerMsg::LogContent { lines } => {
            payload.push(S_LOG_CONTENT);
            payload.extend_from_slice(&(lines.len() as u32).to_le_bytes());
            for line in lines {
                payload.extend_from_slice(&(line.len() as u32).to_le_bytes());
                payload.extend_from_slice(line.as_bytes());
            }
        }
        ServerMsg::ControlNotify { line } => {
            payload.push(S_CONTROL_NOTIFY);
            payload.extend_from_slice(&(line.len() as u32).to_le_bytes());
            payload.extend_from_slice(line.as_bytes());
        }
        ServerMsg::TermOscQuery {
            pane_id,
            code,
            bell_terminated,
        } => {
            payload.push(S_TERM_OSC_QUERY);
            payload.extend_from_slice(&pane_id.to_le_bytes());
            payload.push(*code);
            payload.push(if *bell_terminated { 1 } else { 0 });
        }
        ServerMsg::PskUpdated => {
            payload.push(S_PSK_UPDATED);
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
            let attach = r.first().copied().unwrap_or(1) != 0;
            if !r.is_empty() {
                r = &r[1..];
            }
            let auth_token = read_optional_string(&mut r)?;
            Ok(ClientMsg::Identify {
                rows,
                cols,
                attach,
                auth_token,
            })
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
            let name = if has_name != 0 {
                let len = read_u32(&mut r)? as usize;
                Some(String::from_utf8_lossy(&r[..len]).into_owned())
            } else {
                None
            };
            let has_cwd = read_u8(&mut r)?;
            let cwd = if has_cwd != 0 {
                let len = read_u32(&mut r)? as usize;
                Some(String::from_utf8_lossy(&r[..len]).into_owned())
            } else {
                None
            };
            let has_cmd = read_u8(&mut r)?;
            let command = if has_cmd != 0 {
                let len = read_u32(&mut r)? as usize;
                Some(String::from_utf8_lossy(&r[..len]).into_owned())
            } else {
                None
            };
            Ok(ClientMsg::NewSession { name, cwd, command })
        }
        C_NEXT_SESSION => Ok(ClientMsg::NextSession),
        C_PREV_SESSION => Ok(ClientMsg::PrevSession),
        C_SELECT_SESSION => {
            let len = read_u32(&mut r)? as usize;
            let name = String::from_utf8_lossy(&r[..len]).into_owned();
            Ok(ClientMsg::SelectSession { name })
        }
        C_KILL_SESSION => Ok(ClientMsg::KillSession),
        C_RENAME_SESSION => {
            let len = read_u32(&mut r)? as usize;
            let name = String::from_utf8_lossy(&r[..len]).into_owned();
            Ok(ClientMsg::RenameSession { name })
        }
        C_LIST_SESSIONS => Ok(ClientMsg::ListSessions),
        C_KILL_SERVER => Ok(ClientMsg::KillServer),
        C_NEW_WINDOW_IN => {
            let session = if r.first() == Some(&1) {
                r = &r[1..];
                let len = read_u32(&mut r)? as usize;
                let s = String::from_utf8_lossy(&r[..len]).into_owned();
                r = &r[len..];
                Some(s)
            } else {
                r = &r[1..];
                None
            };
            let command = if r.first() == Some(&1) {
                r = &r[1..];
                let len = read_u32(&mut r)? as usize;
                let c = String::from_utf8_lossy(&r[..len]).into_owned();
                Some(c)
            } else {
                None
            };
            Ok(ClientMsg::NewWindowIn { session, command })
        }
        C_CAPTURE_WINDOW => {
            let session = if r.first() == Some(&1) {
                r = &r[1..];
                let len = read_u32(&mut r)? as usize;
                if r.len() < len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "CaptureWindow session name truncated",
                    ));
                }
                let s = String::from_utf8_lossy(&r[..len]).into_owned();
                r = &r[len..];
                Some(s)
            } else {
                r = &r[1..];
                None
            };
            let window = if r.first() == Some(&1) {
                if r.len() < 2 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "CaptureWindow window index truncated",
                    ));
                }
                let w = r[1];
                r = &r[2..];
                Some(w)
            } else {
                r = &r[1..];
                None
            };
            // Optional trailing format/colors (older clients omit → ascii, no colors).
            let (format, colors) = if r.len() >= 2 {
                let f = r[0];
                let c = r[1] != 0;
                r = &r[2..];
                (f, c)
            } else {
                (0, false)
            };
            // Optional outer-TTY palette (OSC 10/11).
            let (term_fg, term_bg) = if r.first() == Some(&1) {
                r = &r[1..];
                let fg = if r.first() == Some(&1) {
                    if r.len() < 4 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "CaptureWindow term_fg truncated",
                        ));
                    }
                    let rgb = (r[1], r[2], r[3]);
                    r = &r[4..];
                    Some(rgb)
                } else {
                    r = &r[1..];
                    None
                };
                let bg = if r.first() == Some(&1) {
                    if r.len() < 4 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "CaptureWindow term_bg truncated",
                        ));
                    }
                    let rgb = (r[1], r[2], r[3]);
                    Some(rgb)
                } else {
                    None
                };
                (fg, bg)
            } else {
                (None, None)
            };
            Ok(ClientMsg::CaptureWindow {
                session,
                window,
                format,
                colors,
                term_fg,
                term_bg,
            })
        }
        C_SEND_KEYS => {
            let session = if r.first() == Some(&1) {
                r = &r[1..];
                let len = read_u32(&mut r)? as usize;
                let s = String::from_utf8_lossy(&r[..len]).into_owned();
                r = &r[len..];
                Some(s)
            } else {
                r = &r[1..];
                None
            };
            let window = if r.first() == Some(&1) {
                let w = r[1];
                r = &r[2..];
                Some(w)
            } else {
                r = &r[1..];
                None
            };
            let klen = read_u32(&mut r)? as usize;
            let keys = r[..klen].to_vec();
            Ok(ClientMsg::SendKeys {
                session,
                window,
                keys,
            })
        }
        C_GET_LOG => Ok(ClientMsg::GetLog),
        C_IDENTIFY_CONTROL => {
            let rows = read_u16(&mut r)?;
            let cols = read_u16(&mut r)?;
            let auth_token = read_optional_string(&mut r)?;
            Ok(ClientMsg::IdentifyControl {
                rows,
                cols,
                auth_token,
            })
        }
        C_CONTROL_COMMAND => {
            let len = read_u32(&mut r)? as usize;
            let line = String::from_utf8_lossy(&r[..len]).into_owned();
            Ok(ClientMsg::ControlCommand { line })
        }
        C_REFRESH => Ok(ClientMsg::Refresh),
        C_TERM_OSC_REPLY => {
            let pane_id = read_u32(&mut r)?;
            let len = read_u32(&mut r)? as usize;
            if r.len() < len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "TermOscReply truncated",
                ));
            }
            let data = r[..len].to_vec();
            Ok(ClientMsg::TermOscReply { pane_id, data })
        }
        C_TERM_PALETTE => {
            let fg = if r.first() == Some(&1) {
                if r.len() < 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "TermPalette fg truncated",
                    ));
                }
                let rgb = (r[1], r[2], r[3]);
                r = &r[4..];
                Some(rgb)
            } else {
                r = &r[1..];
                None
            };
            let bg = if r.first() == Some(&1) {
                if r.len() < 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "TermPalette bg truncated",
                    ));
                }
                Some((r[1], r[2], r[3]))
            } else {
                None
            };
            Ok(ClientMsg::TermPalette { fg, bg })
        }
        C_SET_PSK => {
            let len = read_u32(&mut r)? as usize;
            let psk = String::from_utf8_lossy(&r[..len]).into_owned();
            Ok(ClientMsg::SetPsk { psk })
        }
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
            let version = if r.is_empty() {
                "unknown".to_string()
            } else {
                let len = read_u32(&mut r)? as usize;
                let s = String::from_utf8_lossy(&r[..len]).into_owned();
                r = &r[len..];
                s
            };
            let address = if r.is_empty() {
                "unknown".to_string()
            } else {
                let len = read_u32(&mut r)? as usize;
                String::from_utf8_lossy(&r[..len]).into_owned()
            };
            Ok(ServerMsg::IdentifyAck {
                rows,
                cols,
                version,
                address,
            })
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
            let replay = read_u8(&mut r)? != 0;
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
            Ok(ServerMsg::ScrollbackUpdate { rows, replay })
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
            let high_output = !r.is_empty() && r[0] != 0;
            if !r.is_empty() {
                r = &r[1..];
            }
            // Optional: older servers stop after `high_output`.
            let server = read_optional_string(&mut r)?;
            Ok(ServerMsg::StatusBarUpdate {
                session,
                windows,
                active,
                session_count,
                high_output,
                server,
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
            let address = if r.is_empty() {
                "unknown".to_string()
            } else {
                let len = read_u32(&mut r)? as usize;
                String::from_utf8_lossy(&r[..len]).into_owned()
            };
            Ok(ServerMsg::SessionList { sessions, address })
        }
        S_WINDOW_CAPTURE => {
            let len = read_u32(&mut r)? as usize;
            let content = String::from_utf8_lossy(&r[..len]).into_owned();
            Ok(ServerMsg::WindowCapture { content })
        }
        S_LOG_CONTENT => {
            let count = read_u32(&mut r)? as usize;
            let mut lines = Vec::with_capacity(count);
            for _ in 0..count {
                let len = read_u32(&mut r)? as usize;
                lines.push(String::from_utf8_lossy(&r[..len]).into_owned());
                r = &r[len..];
            }
            Ok(ServerMsg::LogContent { lines })
        }
        S_CONTROL_NOTIFY => {
            let len = read_u32(&mut r)? as usize;
            let line = String::from_utf8_lossy(&r[..len]).into_owned();
            Ok(ServerMsg::ControlNotify { line })
        }
        S_TERM_OSC_QUERY => {
            let pane_id = read_u32(&mut r)?;
            if r.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "TermOscQuery truncated",
                ));
            }
            let code = r[0];
            let bell_terminated = r.get(1).copied().unwrap_or(1) != 0;
            Ok(ServerMsg::TermOscQuery {
                pane_id,
                code,
                bell_terminated,
            })
        }
        S_PSK_UPDATED => Ok(ServerMsg::PskUpdated),
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

/// Read a length-prefixed UTF-8 string if enough bytes remain; else empty.
/// Used for optional trailing auth_token on Identify (backward compatible).
fn read_optional_string(r: &mut &[u8]) -> io::Result<String> {
    if r.len() < 4 {
        return Ok(String::new());
    }
    let len = read_u32(r)? as usize;
    if len == 0 {
        return Ok(String::new());
    }
    if r.len() < len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated string",
        ));
    }
    let s = String::from_utf8_lossy(&r[..len]).into_owned();
    *r = &r[len..];
    Ok(s)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bar_roundtrip_includes_server_name() {
        let msg = ServerMsg::StatusBarUpdate {
            session: "lrmux".into(),
            windows: vec!["zsh".into()],
            active: 0,
            session_count: 1,
            high_output: false,
            server: "infra".into(),
        };
        let bytes = encode_server(&msg);
        match decode_server(&mut &bytes[..]).unwrap() {
            ServerMsg::StatusBarUpdate {
                session, server, ..
            } => {
                assert_eq!(session, "lrmux");
                assert_eq!(server, "infra");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
