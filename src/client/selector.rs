// Interactive startup selector: discover servers, list sessions, let the user pick.
//
// Uses the shared `inventory` module (local sockets + LAN UDP discovery).

use std::io::{self, Write};
use std::os::fd::AsRawFd;

use crate::client::inventory::{self, ServerEntry};
use crate::client::terminal;
use crate::ipc;

/// What the user chose in the selector.
pub enum SelectorResult {
    /// Attach to an existing session on a server.
    /// `tcp` is set for LAN-discovered remote servers (`host:port`).
    Attach {
        server: String,
        session: String,
        tcp: Option<String>,
    },
    /// Create a new session on a server (optionally named).
    /// `tcp` is set for a LAN server; the caller must not fork a local one.
    NewSession {
        server: String,
        name: Option<String>,
        tcp: Option<String>,
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
    /// When set, connect via this TCP address instead of a local Unix socket.
    tcp: Option<String>,
    lan: bool,
    /// ListSessions failed. Enter reports this and stays in the selector.
    unreachable: Option<String>,
}

impl Entry {
    fn display(&self) -> String {
        if let Some(err) = &self.unreachable {
            let addr = self.tcp.as_deref().unwrap_or("?");
            return format!("{}@{} [lan] unreachable: {err}", self.server, addr);
        }
        if self.lan {
            match &self.tcp {
                Some(addr) => format!("{}@{}/{} [lan]", self.server, addr, self.session),
                None => format!("{}/{} [lan]", self.server, self.session),
            }
        } else {
            format!("{}/{}", self.server, self.session)
        }
    }
}

/// Run the interactive selector. Returns the user's choice.
/// Auto-joins if exactly one server/session exists.
pub fn run_selector() -> io::Result<SelectorResult> {
    run_selector_impl(false)
}

/// Run the interactive selector, forcing the TUI even if only one session exists.
pub fn run_selector_forced() -> io::Result<SelectorResult> {
    run_selector_impl(true)
}

fn run_selector_impl(force: bool) -> io::Result<SelectorResult> {
    let servers = inventory::collect(std::time::Duration::from_millis(500))?;
    let mut entries: Vec<Entry> = Vec::new();
    for s in &servers {
        push_server_entries(&mut entries, s);
    }

    // Fast path: exactly one reachable session → auto-join.
    // "(no sessions)" is not a session name, and an unreachable LAN row
    // must not attach (or spawn anything) on its own.
    if !force && entries.len() == 1 {
        let e = &entries[0];
        if e.unreachable.is_none() && e.session != "(no sessions)" {
            return Ok(SelectorResult::Attach {
                server: e.server.clone(),
                session: e.session.clone(),
                tcp: e.tcp.clone(),
            });
        }
    }

    // No servers running:
    // - Default (non-forced): auto-create "default" server (no prompt).
    // - Forced: show interactive name prompt.
    if entries.is_empty() {
        if force {
            return no_servers_prompt();
        }
        return Ok(SelectorResult::NewServer {
            name: "default".to_string(),
        });
    }

    interactive_selector(entries)
}

fn push_server_entries(entries: &mut Vec<Entry>, s: &ServerEntry) {
    let tcp = s.tcp_addr().map(|a| a.to_string());
    let lan = s.is_lan();
    if let Some(err) = &s.probe_error {
        entries.push(Entry {
            server: s.name.clone(),
            session: "(unreachable)".to_string(),
            tcp,
            lan,
            unreachable: Some(err.clone()),
        });
        return;
    }
    if s.sessions.is_empty() {
        // Still show the server so the user can create a session on it.
        entries.push(Entry {
            server: s.name.clone(),
            session: "(no sessions)".to_string(),
            tcp,
            lan,
            unreachable: None,
        });
        return;
    }
    for session in &s.sessions {
        entries.push(Entry {
            server: s.name.clone(),
            session: session.clone(),
            tcp: tcp.clone(),
            lan,
            unreachable: None,
        });
    }
}

/// Default session name based on the current directory basename.
fn default_session_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "session".to_string())
}

/// Prompt when no servers are running.
/// Shows an editable name field for the new server.
fn no_servers_prompt() -> io::Result<SelectorResult> {
    let default_name = default_session_name();
    match name_prompt(
        "lrmux — no servers running",
        "Server name",
        &default_name,
        "Enter=create  Esc=quit",
        None,
    ) {
        Some((name, _)) => Ok(SelectorResult::NewServer { name }),
        None => Ok(SelectorResult::Quit),
    }
}

/// Shared editable name prompt. Returns (name, optional extra) or None if cancelled.
fn name_prompt(
    title: &str,
    label: &str,
    default: &str,
    hint: &str,
    _extra: Option<&str>,
) -> Option<(String, ())> {
    let _raw = terminal::enter_raw_mode().ok()?;
    let mut stdout = io::stdout();
    let mut name = default.to_string();
    loop {
        let _ = write!(stdout, "\x1b[2J\x1b[H");
        let _ = write!(stdout, "{title}\r\n");
        let _ = write!(stdout, "\x1b[90m──────────────────────────────\x1b[0m\r\n");
        let _ = write!(stdout, "{label}: {name}\r\n");
        let _ = write!(stdout, "\x1b[90m{hint}\x1b[0m\r\n");
        let _ = stdout.flush();

        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(io::stdin().as_raw_fd(), buf.as_mut_ptr() as *mut _, 1) };
        if n <= 0 {
            return None;
        }
        match buf[0] {
            b'\r' | b'\n' => {
                if name.is_empty() {
                    name = default.to_string();
                }
                return Some((name, ()));
            }
            0x03 | 0x1b => return None,
            0x7f | 0x08 => {
                name.pop();
            }
            b if b.is_ascii_graphic() || b == b' ' => {
                name.push(b as char);
            }
            _ => {}
        }
    }
}

/// The interactive selector TUI.
fn interactive_selector(entries: Vec<Entry>) -> io::Result<SelectorResult> {
    let _raw_guard = terminal::enter_raw_mode()?;
    let mut stdout = io::stdout();

    let mut selected: usize = 0;
    let mut query: String = String::new();
    let mut flash: Option<String> = None;

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

        write!(stdout, "\x1b[2J\x1b[H")?;
        write!(stdout, "lrmux — select a session\r\n")?;
        write!(stdout, "\x1b[90m──────────────────────────────\x1b[0m\r\n")?;
        if filt.is_empty() {
            write!(stdout, "\x1b[90m  (no matches)\x1b[0m\r\n")?;
        } else {
            for (row, &idx) in filt.iter().enumerate() {
                let e = &entries[idx];
                if row == selected {
                    write!(stdout, "\x1b[1;36m▶ {}\x1b[0m\r\n", e.display())?;
                } else {
                    write!(stdout, "  {}\r\n", e.display())?;
                }
            }
        }
        write!(stdout, "\x1b[90m──────────────────────────────\x1b[0m\r\n")?;
        if let Some(msg) = &flash {
            write!(stdout, "\x1b[31m{msg}\x1b[0m\r\n")?;
        }
        write!(
            stdout,
            "\x1b[90mEnter=join  n=new session  N=new server  q=quit\x1b[0m\r\n"
        )?;
        write!(stdout, "filter> {}", query)?;
        stdout.flush()?;

        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(io::stdin().as_raw_fd(), buf.as_mut_ptr() as *mut _, 1) };
        if n <= 0 {
            break;
        }
        let key = buf[0];

        match key {
            b'\r' | b'\n' => {
                if let Some(&idx) = filt.get(selected) {
                    let e = &entries[idx];
                    if let Some(err) = &e.unreachable {
                        let addr = e.tcp.as_deref().unwrap_or("?");
                        flash = Some(format!("cannot reach {} @ {addr}: {err}", e.server));
                        continue;
                    }
                    if e.session == "(no sessions)" {
                        return Ok(SelectorResult::NewSession {
                            server: e.server.clone(),
                            name: None,
                            tcp: e.tcp.clone(),
                        });
                    }
                    return Ok(SelectorResult::Attach {
                        server: e.server.clone(),
                        session: e.session.clone(),
                        tcp: e.tcp.clone(),
                    });
                }
            }
            b'q' | 0x03 => {
                return Ok(SelectorResult::Quit);
            }
            0x1b => {
                let mut seq = [0u8; 2];
                let n2 =
                    unsafe { libc::read(io::stdin().as_raw_fd(), seq.as_mut_ptr() as *mut _, 2) };
                if n2 == 2 && seq[0] == b'[' {
                    match seq[1] {
                        b'A' if selected > 0 => selected -= 1,
                        b'B' if selected + 1 < filt.len() => selected += 1,
                        _ => {}
                    }
                } else {
                    return Ok(SelectorResult::Quit);
                }
            }
            b'j' if selected + 1 < filt.len() => selected += 1,
            b'k' => selected = selected.saturating_sub(1),
            b'n' => {
                let (server, tcp) = filt
                    .get(selected)
                    .map(|&i| (entries[i].server.clone(), entries[i].tcp.clone()))
                    .unwrap_or_else(|| ("default".to_string(), None));
                if let Some(err) = filt
                    .get(selected)
                    .and_then(|&i| entries[i].unreachable.clone())
                {
                    flash = Some(format!("cannot reach {server}: {err}"));
                    continue;
                }
                let default_name = default_session_name();
                if let Some((name, _)) = name_prompt(
                    "lrmux — new session",
                    "Session name",
                    &default_name,
                    "Enter=create  Esc=cancel",
                    None,
                ) {
                    return Ok(SelectorResult::NewSession {
                        server,
                        name: Some(name),
                        tcp,
                    });
                }
            }
            b'N' => {
                let default_name = ipc::auto_server_name();
                if let Some((name, _)) = name_prompt(
                    "lrmux — new server",
                    "Server name",
                    &default_name,
                    "Enter=create  Esc=cancel",
                    None,
                ) {
                    return Ok(SelectorResult::NewServer { name });
                }
            }
            0x7f | 0x08 => {
                query.pop();
                selected = 0;
            }
            b if b.is_ascii_graphic() => {
                query.push(b as char);
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
