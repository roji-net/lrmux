// Interactive startup selector: discover servers, list sessions, let the user pick.
//
// Uses the shared `inventory` module (local sockets + LAN UDP discovery).
// Renders an ASCII table grouped by Local Machine / remote servers.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::inventory::{self, ServerEntry};
use crate::client::terminal;
use crate::ipc;
use crate::proto::SessionInfo;

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

/// A single selectable row in the selector list.
#[derive(Clone)]
struct Entry {
    server: String,
    session: String,
    /// When set, connect via this TCP address instead of a local Unix socket.
    tcp: Option<String>,
    /// ListSessions failed. Enter reports this and stays in the selector.
    unreachable: Option<String>,
    attached: u16,
    created: u64,
    last_activity: u64,
    has_activity: bool,
    /// Group key for headers: "local" or remote identity.
    group_key: String,
    /// Colored header label for this entry's group.
    group_label: String,
    /// ANSI color for the group header (empty for local).
    group_color: String,
}

impl Entry {
    fn search_text(&self) -> String {
        format!(
            "{}/{} {} {}",
            self.server,
            self.session,
            self.group_label,
            self.tcp.as_deref().unwrap_or("")
        )
    }

    fn status_label(&self) -> &'static str {
        if self.unreachable.is_some() {
            "unreachable"
        } else if self.session == "(no sessions)" {
            "empty"
        } else if self.attached > 0 {
            "attached"
        } else {
            "detached"
        }
    }

    /// `server/session` locally, `server:port/session` on remotes.
    fn session_cell(&self) -> String {
        match self.tcp.as_deref().and_then(port_of_addr) {
            Some(port) => format!("{}:{}/{}", self.server, port, self.session),
            None => format!("{}/{}", self.server, self.session),
        }
    }
}

/// Colors cycled for remote server group headers.
const REMOTE_COLORS: &[&str] = &[
    "\x1b[1;36m", // bright cyan
    "\x1b[1;35m", // bright magenta
    "\x1b[1;32m", // bright green
    "\x1b[1;33m", // bright yellow
    "\x1b[1;34m", // bright blue
    "\x1b[1;91m", // bright red
];

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
    push_all_entries(&mut entries, &servers);

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

fn push_all_entries(entries: &mut Vec<Entry>, servers: &[ServerEntry]) {
    // This machine first: Unix sockets + LAN announces whose IP is ours
    // (127.0.0.1, ::1, or a local interface — not "other computers").
    for s in servers.iter().filter(|s| s.is_this_machine()) {
        push_server_entries(
            entries,
            s,
            "local".to_string(),
            "Local Machine".to_string(),
            "\x1b[1;37m".to_string(),
        );
    }

    // Other machines, grouped by host/IP (port lives on the row, not the header).
    let mut remotes: Vec<&ServerEntry> = servers.iter().filter(|s| !s.is_this_machine()).collect();
    remotes.sort_by(|a, b| {
        let ha = a
            .tcp_addr()
            .map(inventory::host_of_addr)
            .unwrap_or_default();
        let hb = b
            .tcp_addr()
            .map(inventory::host_of_addr)
            .unwrap_or_default();
        ha.cmp(&hb).then_with(|| a.name.cmp(&b.name))
    });

    let mut last_host = String::new();
    let mut color_i = 0usize;
    let mut group_color = String::new();
    for s in remotes {
        let host = s
            .tcp_addr()
            .map(inventory::host_of_addr)
            .unwrap_or_else(|| "?".into());
        if host != last_host {
            group_color = REMOTE_COLORS[color_i % REMOTE_COLORS.len()].to_string();
            color_i += 1;
            last_host = host.clone();
        }
        push_server_entries(
            entries,
            s,
            format!("lan:{host}"),
            format!("Remote · {host}"),
            group_color.clone(),
        );
    }
}

/// Port part of `host:port` or `[ipv6]:port`, if present.
fn port_of_addr(addr: &str) -> Option<String> {
    inventory::port_of_addr(addr)
}

fn push_server_entries(
    entries: &mut Vec<Entry>,
    s: &ServerEntry,
    group_key: String,
    group_label: String,
    group_color: String,
) {
    let tcp = s.tcp_addr().map(|a| a.to_string());
    if let Some(err) = &s.probe_error {
        entries.push(Entry {
            server: s.name.clone(),
            session: "(unreachable)".to_string(),
            tcp,
            unreachable: Some(err.clone()),
            attached: 0,
            created: 0,
            last_activity: 0,
            has_activity: false,
            group_key,
            group_label,
            group_color,
        });
        return;
    }
    if s.sessions.is_empty() {
        entries.push(Entry {
            server: s.name.clone(),
            session: "(no sessions)".to_string(),
            tcp,
            unreachable: None,
            attached: 0,
            created: 0,
            last_activity: 0,
            has_activity: false,
            group_key,
            group_label,
            group_color,
        });
        return;
    }
    for info in &s.sessions {
        entries.push(entry_from_info(
            s,
            info,
            tcp.clone(),
            group_key.clone(),
            group_label.clone(),
            group_color.clone(),
        ));
    }
}

fn entry_from_info(
    s: &ServerEntry,
    info: &SessionInfo,
    tcp: Option<String>,
    group_key: String,
    group_label: String,
    group_color: String,
) -> Entry {
    Entry {
        server: s.name.clone(),
        session: info.name.clone(),
        tcp,
        unreachable: None,
        attached: info.attached,
        created: info.created,
        last_activity: info.last_activity,
        has_activity: info.has_activity,
        group_key,
        group_label,
        group_color,
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

/// Format a unix timestamp as a compact relative string.
fn format_ago(ts: u64) -> String {
    if ts == 0 {
        return "—".to_string();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(ts);
    let ago = now.saturating_sub(ts);
    if ago < 60 {
        "just now".to_string()
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 86400 {
        format!("{}h ago", ago / 3600)
    } else if ago < 86400 * 14 {
        format!("{}d ago", ago / 86400)
    } else {
        // Absolute-ish compact: days since epoch isn't helpful; show date via local.
        format_absolute(ts)
    }
}

fn format_absolute(ts: u64) -> String {
    // Manual UTC calendar (no chrono dependency). Good enough for the table.
    // Algorithm from civil_from_days (Howard Hinnant).
    let z = (ts / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let hour = (ts / 3600) % 24;
    let min = (ts / 60) % 60;
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}")
}

fn pad_visible(s: &str, width: usize) -> String {
    let vis = s.chars().count();
    if vis >= width {
        s.chars().take(width).collect()
    } else {
        format!("{s}{}", " ".repeat(width - vis))
    }
}

/// The interactive selector TUI — ASCII table grouped by server.
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
                .filter(|&i| fuzzy_match(&entries[i].search_text(), q))
                .collect()
        }
    };

    loop {
        let filt = filtered(&query);

        write!(stdout, "\x1b[2J\x1b[H")?;
        write!(stdout, "\x1b[1mlrmux\x1b[0m — select a session\r\n")?;

        // Column widths.
        const W_NUM: usize = 3;
        const W_ACT: usize = 2;
        const W_SESS: usize = 34;
        const W_STAT: usize = 11;
        const W_CREATED: usize = 12;
        const W_LAST: usize = 12;

        // Header.
        write!(
            stdout,
            "\x1b[90m{} {} {} {} {} {}\x1b[0m\r\n",
            pad_visible("#", W_NUM),
            pad_visible("●", W_ACT),
            pad_visible("Server/Session", W_SESS),
            pad_visible("Status", W_STAT),
            pad_visible("Created", W_CREATED),
            pad_visible("Activity", W_LAST),
        )?;
        write!(
            stdout,
            "\x1b[90m{}\x1b[0m\r\n",
            "─".repeat(W_NUM + 1 + W_ACT + 1 + W_SESS + 1 + W_STAT + 1 + W_CREATED + 1 + W_LAST)
        )?;

        if filt.is_empty() {
            write!(stdout, "\x1b[90m  (no matches)\x1b[0m\r\n")?;
        } else {
            let mut last_group: Option<&str> = None;
            for (row, &idx) in filt.iter().enumerate() {
                let e = &entries[idx];
                if last_group != Some(e.group_key.as_str()) {
                    write!(
                        stdout,
                        "{}── {} ──\x1b[0m\r\n",
                        e.group_color, e.group_label
                    )?;
                    last_group = Some(e.group_key.as_str());
                }

                let num = if row < 10 {
                    format!("{row}")
                } else {
                    String::new()
                };
                let bullet = if e.has_activity { "●" } else { " " };
                let sess = e.session_cell();
                let status = e.status_label();
                let created = format_ago(e.created);
                let activity = format_ago(e.last_activity);

                let status_cell = {
                    let plain = pad_visible(status, W_STAT);
                    match status {
                        "attached" => format!("\x1b[32m{plain}\x1b[0m"),
                        "detached" => format!("\x1b[90m{plain}\x1b[0m"),
                        "unreachable" => format!("\x1b[31m{plain}\x1b[0m"),
                        _ => plain,
                    }
                };

                let bullet_cell = if e.has_activity {
                    format!("\x1b[1;31m{}\x1b[0m", pad_visible(bullet, W_ACT))
                } else {
                    pad_visible(bullet, W_ACT)
                };

                let line = format!(
                    "{} {} {} {} {} {}",
                    pad_visible(&num, W_NUM),
                    bullet_cell,
                    pad_visible(&sess, W_SESS),
                    status_cell,
                    pad_visible(&created, W_CREATED),
                    pad_visible(&activity, W_LAST),
                );

                if row == selected {
                    write!(stdout, "\x1b[1;36m▶\x1b[0m {line}\r\n")?;
                } else {
                    write!(stdout, "  {line}\r\n")?;
                }
            }
        }

        write!(
            stdout,
            "\x1b[90m{}\x1b[0m\r\n",
            "─".repeat(W_NUM + 1 + W_ACT + 1 + W_SESS + 1 + W_STAT + 1 + W_CREATED + 1 + W_LAST)
        )?;
        if let Some(msg) = &flash {
            write!(stdout, "\x1b[31m{msg}\x1b[0m\r\n")?;
        }
        write!(
            stdout,
            "\x1b[90mEnter=join  0-9=jump  n=new session  N=new server  q=quit\x1b[0m\r\n"
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
            b'0'..=b'9' => {
                let jump = (key - b'0') as usize;
                if let Some(&idx) = filt.get(jump) {
                    let e = &entries[idx];
                    if let Some(err) = &e.unreachable {
                        let addr = e.tcp.as_deref().unwrap_or("?");
                        flash = Some(format!("cannot reach {} @ {addr}: {err}", e.server));
                        selected = jump;
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
            b if b.is_ascii_alphabetic()
                || b == b'/'
                || b == b'-'
                || b == b'_'
                || b == b'.'
                || b == b' '
                || b == b'@' =>
            {
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
