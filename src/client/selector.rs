// Interactive startup selector: discover servers, list sessions, let the user pick.
//
// Shows a flat list of `server / session` entries with a fuzzy filter.
// j/k or arrow keys to navigate, Enter to join, n for new session, N for new server.

use std::io::{self, Write};
use std::os::fd::AsRawFd;

use crate::client::terminal;
use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};

/// What the user chose in the selector.
pub enum SelectorResult {
    /// Attach to an existing session on a server.
    Attach { server: String, session: String },
    /// Create a new session on a server (optionally named).
    NewSession {
        server: String,
        name: Option<String>,
    },
    /// Create a new server + session.
    NewServer { name: String },
    /// User cancelled / quit.
    Quit,
}

/// A single entry in the selector list.
#[derive(Clone)]
struct Entry {
    server: String,
    session: String,
}

/// Run the interactive selector. Returns the user's choice.
pub fn run_selector() -> io::Result<SelectorResult> {
    // Discover all running servers and their sessions.
    let servers = discover_servers();
    let mut entries: Vec<Entry> = Vec::new();
    for server in &servers {
        if let Ok(sessions) = query_sessions(server) {
            for session in sessions {
                entries.push(Entry {
                    server: server.clone(),
                    session,
                });
            }
        }
    }

    // Fast path: exactly one server with one session → auto-join.
    if entries.len() == 1 {
        return Ok(SelectorResult::Attach {
            server: entries[0].server.clone(),
            session: entries[0].session.clone(),
        });
    }

    // No servers running → prompt to create one.
    if entries.is_empty() {
        return no_servers_prompt();
    }

    // Interactive selector.
    interactive_selector(entries)
}

/// Discover all running lrmux servers by scanning the socket directory.
fn discover_servers() -> Vec<String> {
    let uid = unsafe { libc::getuid() };
    let dir = format!("/tmp/lrmux-{uid}");
    let mut servers = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if ipc::server_exists(&path)
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                servers.push(name.to_string());
            }
        }
    }
    servers.sort();
    servers
}

/// Query a server for its session list (lightweight: connect, ListSessions, disconnect).
fn query_sessions(server_name: &str) -> io::Result<Vec<String>> {
    let sock = ipc::socket_path(server_name);
    let mut stream = ipc::connect(&sock)?;

    // Send ListSessions directly (no Identify needed for query).
    let msg = proto::encode_client(&ClientMsg::ListSessions);
    proto::send(&mut stream, &msg)?;

    // Read the SessionList response.
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::SessionList { sessions }) => Ok(sessions),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected SessionList",
        )),
        Err(e) => Err(e),
    }
}

/// Prompt when no servers are running.
fn no_servers_prompt() -> io::Result<SelectorResult> {
    // Simple: just create a default server + session.
    Ok(SelectorResult::NewServer {
        name: "default".to_string(),
    })
}

/// The interactive selector TUI.
fn interactive_selector(entries: Vec<Entry>) -> io::Result<SelectorResult> {
    let _raw_guard = terminal::enter_raw_mode()?;
    let mut stdout = io::stdout();

    let mut selected: usize = 0;
    let mut query: String = String::new();

    // Filtered entries (recomputed when query changes).
    let filtered = |q: &str| -> Vec<usize> {
        if q.is_empty() {
            (0..entries.len()).collect()
        } else {
            (0..entries.len())
                .filter(|&i| fuzzy_match(&entries[i].display(), q))
                .collect()
        }
    };

    loop {
        let filt = filtered(&query);

        // Render. Use \r\n because raw mode doesn't translate \n to \r\n.
        write!(stdout, "\x1b[2J\x1b[H")?; // clear + home
        write!(stdout, "lrmux — select a session\r\n")?;
        write!(stdout, "\x1b[90m──────────────────────────────\x1b[0m\r\n")?;
        if filt.is_empty() {
            write!(stdout, "\x1b[90m  (no matches)\x1b[0m\r\n")?;
        } else {
            for (row, &idx) in filt.iter().enumerate() {
                let e = &entries[idx];
                if row == selected {
                    write!(stdout, "\x1b[1;36m▶ {}/{}\x1b[0m\r\n", e.server, e.session)?;
                } else {
                    write!(stdout, "  {}/{}\r\n", e.server, e.session)?;
                }
            }
        }
        write!(stdout, "\x1b[90m──────────────────────────────\x1b[0m\r\n")?;
        write!(
            stdout,
            "\x1b[90mEnter=join  n=new session  N=new server  q=quit\x1b[0m\r\n"
        )?;
        write!(stdout, "filter> {}", query)?;
        stdout.flush()?;

        // Read a key.
        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(io::stdin().as_raw_fd(), buf.as_mut_ptr() as *mut _, 1) };
        if n <= 0 {
            break;
        }
        let key = buf[0];

        match key {
            // Enter → join selected.
            b'\r' | b'\n' => {
                if let Some(&idx) = filt.get(selected) {
                    return Ok(SelectorResult::Attach {
                        server: entries[idx].server.clone(),
                        session: entries[idx].session.clone(),
                    });
                }
            }
            // q or Ctrl-C → quit.
            b'q' | 0x03 => {
                return Ok(SelectorResult::Quit);
            }
            // Esc → quit.
            0x1b => {
                // Check for arrow key escape sequence.
                let mut seq = [0u8; 2];
                let n2 =
                    unsafe { libc::read(io::stdin().as_raw_fd(), seq.as_mut_ptr() as *mut _, 2) };
                if n2 == 2 && seq[0] == b'[' {
                    match seq[1] {
                        // Up
                        b'A' if selected > 0 => {
                            selected -= 1;
                        }
                        // Down
                        b'B' if selected + 1 < filt.len() => {
                            selected += 1;
                        }
                        _ => {}
                    }
                } else {
                    // Plain Esc → quit.
                    return Ok(SelectorResult::Quit);
                }
            }
            // j → down.
            b'j' if selected + 1 < filt.len() => {
                selected += 1;
            }
            // k → up.
            b'k' => {
                selected = selected.saturating_sub(1);
            }
            // n → new session on the highlighted server (or default).
            b'n' => {
                let server = filt
                    .get(selected)
                    .map(|&i| entries[i].server.clone())
                    .unwrap_or_else(|| "default".to_string());
                return Ok(SelectorResult::NewSession { server, name: None });
            }
            // N → new server + session.
            b'N' => {
                return Ok(SelectorResult::NewServer {
                    name: crate::ipc::auto_server_name(),
                });
            }
            // Backspace → remove last char from query.
            0x7f | 0x08 => {
                query.pop();
                selected = 0;
            }
            // Printable ASCII → append to query.
            0x20..=0x7e => {
                query.push(key as char);
                selected = 0;
            }
            _ => {}
        }
    }

    Ok(SelectorResult::Quit)
}

/// Simple subsequence fuzzy match: each char of `query` must appear in `text` in order.
fn fuzzy_match(text: &str, query: &str) -> bool {
    let mut chars = text.chars();
    for qc in query.chars() {
        if !chars.any(|c| c.eq_ignore_ascii_case(&qc)) {
            return false;
        }
    }
    true
}

impl Entry {
    fn display(&self) -> String {
        format!("{}/{}", self.server, self.session)
    }
}
