// lrmux — a modern, fast, minimal-dependency terminal multiplexer

// Phase 3: client/server split. The binary tries to connect to an existing
// server; if none exists, it forks a server process and then connects as a client.

#![allow(dead_code)]

mod client;
mod cmd;
mod config;
mod grid;
mod ipc;
mod keys;
mod layout;
mod log;
mod proto;
mod pty;
mod server;
mod statusbar;
mod term;
mod version;
mod vt;

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::client::selector::SelectorResult;
use crate::ipc::socket_path;
use crate::proto::{ClientMsg, ServerMsg};

/// Connect to a server. Uses TCP if --tcp was set, otherwise Unix socket.
/// `LRMUX_CLI_SERVER` overrides the default server name for CLI helpers
/// (capture-pane, send-keys, …) so tests can target a throwaway server
/// without touching the user's `default`.
fn connect_to_server(server: &str) -> io::Result<crate::ipc::ConnStream> {
    if let Some(addr) = crate::ipc::tcp_addr() {
        return crate::ipc::connect_tcp(&addr);
    }
    let server = if server == "default" {
        std::env::var("LRMUX_CLI_SERVER").unwrap_or_else(|_| server.to_string())
    } else {
        server.to_string()
    };
    let sock = socket_path(&server);
    if !crate::ipc::server_exists(&sock) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("server '{server}' is not running"),
        ));
    }
    crate::ipc::connect(&sock).map(crate::ipc::ConnStream::Unix)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("lrmux: {e}");
        std::process::exit(1);
    }
}

/// CLI arguments.
enum CliAction {
    /// Default: show the selector (or auto-join if exactly one server/session).
    Default,
    /// `lrmux -CC [-t <target>]`: tmux control mode for iTerm2 integration.
    ControlMode { target: Option<String> },
    /// `lrmux -- <cmd>`: create a new window running <cmd> (non-interactive when nested).
    RunCommand(String),
    /// `session-selector` / `ss`: force the interactive selector (no auto-join).
    SessionSelector,
    /// `new-session -s <name> -c <cwd> [-- <cmd>]`
    NewSession {
        name: Option<String>,
        cwd: Option<String>,
        command: Option<String>,
        detached: bool,
    },
    /// `attach [-s <server>] [{-t <target> | <target>}]`
    /// target: [server][:][session], or a TCP `host:port`.
    /// `tcp` is set when the target is a remote address.
    AttachSession {
        server: Option<String>,
        session: Option<String>,
        tcp: Option<String>,
    },
    /// `select-session -t <name>`: switch the interactive client to a session.
    SelectSession(String),
    /// `kill-session -t <name>`: kill a session.
    KillSession(Option<String>),
    /// `new-server -s <name> [--tcp addr] [--ws addr] [--headless] [-CC] [--] [<cmd>]`.
    /// Attaches unless `--headless` (or the `start-server` alias) is set.
    /// `--tcp` / `--ws` are listen addresses, not client connect targets.
    NewServer {
        name: String,
        /// First session name when the user passed `-s`. `None` for an
        /// auto-picked server name, which leaves the session as the cwd.
        session: Option<String>,
        control: bool,
        command: Option<String>,
        headless: bool,
        tcp: Option<String>,
        ws: Option<String>,
    },
    /// `discover`: UDP broadcast probe for LAN servers.
    Discover,
    /// `psk show|set|generate`: manage the TCP pre-shared key.
    Psk(Vec<String>),
    /// `list-servers`: list all running servers.
    ListServers,
    /// `list-sessions [server]`: list sessions (all servers, or one).
    ListSessions(Option<String>),
    /// `list-windows -t <target>`: list windows in a session.
    ListWindows(Option<String>),
    /// `kill-server -t <name>`: kill a named server.
    KillServer(String),
    /// `new-window -t <target> -n <name> -c <cwd> [-- <cmd>]`
    NewWindow {
        target: cmd::Target,
        name: Option<String>,
        cwd: Option<String>,
        command: Option<String>,
    },
    /// `kill-window -t <target>`
    KillWindow(cmd::Target),
    /// `capture-pane -t <target> [-p] [-c|--colors] [--format …] [--clipboard] [--file path]`
    CapturePane {
        target: cmd::Target,
        print: bool,
        colors: bool,
        format: crate::server::CaptureFormat,
        clipboard: bool,
        file: Option<String>,
    },
    /// `send-keys -t <target> <keys> -q`: send keys to a pane's PTY.
    SendKeys {
        target: cmd::Target,
        keys: Vec<u8>,
        quiet: bool,
    },
    /// `select-window -t <target>`
    SelectWindow(cmd::Target),
    /// `rename-window -t <target> <name>`
    RenameWindow { target: cmd::Target, name: String },
    /// `versions`: show client and all server versions.
    Versions,
    /// `-v` / `--version`: this binary's version only.
    ClientVersion,
    /// `--help` / `-h`, or `help [command]`. `Some` is the command topic.
    Help(Option<String>),
    /// Unrecognized subcommand — must not fall through to Default
    /// (inside a pane, Default creates a new window).
    Unknown(String),
}

/// Print usage information. `topic` selects one command (`attach`, an alias, …).
fn print_help(topic: Option<&str>) -> io::Result<()> {
    match topic {
        None => {
            println!("{}", cmd::format_global_help());
            Ok(())
        }
        Some(name) => match cmd::format_command_help(name) {
            Ok(text) => {
                println!("{text}");
                Ok(())
            }
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidInput, e)),
        },
    }
}

/// First positional token, skipping `--tcp`/`--psk` and their values.
fn first_subcommand(args: &[String]) -> Option<&str> {
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--tcp" | "--psk" => i += 2,
            other if other.starts_with('-') => i += 1,
            other => return Some(other),
        }
    }
    None
}

/// Parse CLI arguments into an action.
fn parse_args() -> CliAction {
    let args: Vec<String> = std::env::args().collect();
    // `new-server` / `start-server` use `--tcp` as a listen address. Every
    // other command uses it as the client connect target.
    let owns_listen_tcp =
        first_subcommand(&args).is_some_and(|c| cmd::canonical_name(c) == "new-server");
    // Extract --tcp <addr> flag if present (global, for CLI commands).
    let mut tcp_addr: Option<String> = None;
    let mut filtered: Vec<String> = vec![args[0].clone()];
    let mut i = 1;
    while i < args.len() {
        if !owns_listen_tcp && args[i] == "--tcp" && i + 1 < args.len() {
            tcp_addr = Some(args[i + 1].clone());
            i += 2;
        } else if args[i] == "--psk" && i + 1 < args.len() {
            crate::config::set_psk_override(Some(args[i + 1].clone()));
            i += 2;
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }
    let args = filtered;
    // Store tcp_addr in a global for CLI commands and interactive clients.
    if let Some(addr) = tcp_addr {
        crate::ipc::set_tcp_addr(Some(addr));
    }

    let subcmd = args.get(1).map(|s| s.as_str());
    let subcmd_args = if args.len() > 2 { &args[2..] } else { &[] };

    // `-h`/`--help` after a subcommand shows that command's help
    // (except after `--`, where args belong to the wrapped command).
    if subcmd != Some("--")
        && subcmd != Some("help")
        && subcmd_args.iter().any(|a| a == "-h" || a == "--help")
    {
        return CliAction::Help(subcmd.map(|s| s.to_string()));
    }

    match subcmd {
        Some("--help") | Some("-h") => CliAction::Help(None),
        Some("--version") | Some("-v") => CliAction::ClientVersion,
        Some("help") => {
            let topic = subcmd_args
                .iter()
                .find(|a| a.as_str() != "-h" && a.as_str() != "--help");
            CliAction::Help(topic.cloned())
        }
        Some("-CC") | Some("control-mode") | Some("control") => {
            let parsed = cmd::parse_flags(subcmd_args);
            CliAction::ControlMode {
                target: parsed
                    .get("s")
                    .or_else(|| parsed.get("server"))
                    .or_else(|| parsed.get("t"))
                    .or_else(|| parsed.get("target"))
                    .map(|s| s.to_string()),
            }
        }
        Some("--") => {
            // `lrmux -- <cmd> [args]` — run a command in a new window.
            let cmd = args[2..].join(" ");
            if cmd.is_empty() {
                CliAction::Help(None)
            } else {
                CliAction::RunCommand(cmd)
            }
        }
        Some("session-selector") | Some("ss") => CliAction::SessionSelector,
        Some("new-session") | Some("new") => parse_new_session(subcmd_args),
        Some("attach-session") | Some("attach") => parse_attach(subcmd_args),
        Some("select-session") => {
            let parsed = cmd::parse_flags(subcmd_args);
            CliAction::SelectSession(
                parsed
                    .get("t")
                    .or_else(|| parsed.get("target"))
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            )
        }
        Some("kill-session") => {
            let parsed = cmd::parse_flags(subcmd_args);
            CliAction::KillSession(
                parsed
                    .get("t")
                    .or_else(|| parsed.get("target"))
                    .map(|s| s.to_string()),
            )
        }
        Some("new-server") => parse_new_server(subcmd_args, false),
        Some("start-server") => parse_new_server(subcmd_args, true),
        Some("list-servers") | Some("ls-servers") => CliAction::ListServers,
        Some("discover") => CliAction::Discover,
        Some("psk") => CliAction::Psk(subcmd_args.to_vec()),
        Some("list-sessions") | Some("ls-sessions") | Some("ls") => {
            let parsed = cmd::parse_flags(subcmd_args);
            let server = parsed
                .get("s")
                .or_else(|| parsed.get("server"))
                .map(|s| s.to_string())
                .or_else(|| parsed.positional.first().cloned());
            if let Some(ref name) = server {
                note_tcp_server(name);
            }
            CliAction::ListSessions(server)
        }
        Some("list-windows") | Some("lsw") => {
            let parsed = cmd::parse_flags(subcmd_args);
            CliAction::ListWindows(
                parsed
                    .get("t")
                    .or_else(|| parsed.get("target"))
                    .map(|s| s.to_string()),
            )
        }
        Some("kill-server") => {
            let parsed = cmd::parse_flags(subcmd_args);
            let name = parsed
                .get("s")
                .or_else(|| parsed.get("server"))
                .map(|s| s.to_string())
                .or_else(|| parsed.positional.first().cloned())
                .unwrap_or_else(|| "default".to_string());
            note_tcp_server(&name);
            CliAction::KillServer(name)
        }
        Some("new-window") | Some("neww") => parse_new_window(subcmd_args),
        Some("kill-window") | Some("killw") => {
            let parsed = cmd::parse_flags(subcmd_args);
            CliAction::KillWindow(parsed.target())
        }
        Some("select-window") | Some("selectw") => {
            let parsed = cmd::parse_flags(subcmd_args);
            CliAction::SelectWindow(parsed.target())
        }
        Some("rename-window") | Some("renamew") => {
            let parsed = cmd::parse_flags(subcmd_args);
            let name = parsed.positional.first().cloned().unwrap_or_default();
            CliAction::RenameWindow {
                target: parsed.target(),
                name,
            }
        }
        Some("capture-pane") | Some("capturep") | Some("capture-window") => {
            let parsed = cmd::parse_flags(subcmd_args);
            // `-c` / `--colors`: include cell styles. Independent of --format.
            // (Do not treat `-c` as boolean globally — new-session uses `-c` for cwd.)
            let colors = parsed.has("colors") || parsed.flags.contains_key("c");
            let format = match parsed.get("format") {
                Some(s) => match crate::server::CaptureFormat::parse(s) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("lrmux: {e}");
                        std::process::exit(1);
                    }
                },
                // tmux `-e` ⇒ ansi container with colors
                None if subcmd_args.iter().any(|a| {
                    a.len() >= 2
                        && a.starts_with('-')
                        && !a.starts_with("--")
                        && a[1..].contains('e')
                }) =>
                {
                    crate::server::CaptureFormat::Ansi
                }
                None => crate::server::CaptureFormat::Ascii,
            };
            let colors = colors
                || subcmd_args.iter().any(|a| {
                    a.len() >= 2
                        && a.starts_with('-')
                        && !a.starts_with("--")
                        && a[1..].contains('e')
                });
            CliAction::CapturePane {
                target: parsed.target(),
                print: parsed.has("p") || parsed.has("print"),
                colors,
                format,
                clipboard: parsed.has("clipboard"),
                file: parsed
                    .get("file")
                    .or_else(|| parsed.get("o"))
                    .map(|s| s.to_string()),
            }
        }
        Some("send-keys") | Some("send") => parse_send_keys(subcmd_args),
        Some("versions") | Some("version") => CliAction::Versions,
        // Bare `lrmux` (no subcommand) → selector / attach.
        None => CliAction::Default,
        // Anything else is a hard error — never treat typos as Default,
        // because nested Default spawns a new window.
        Some(other) => CliAction::Unknown(other.to_string()),
    }
}

/// Parse `new-session` args using tmux-style flags.
///   new-session [-s name] [-c cwd] [-d] [--] [shell-command...]
///
/// The shell-command may be given after `--` or as positional args
/// (tmux-compatible). It is run via `$SHELL -ci`. Note: `-c` is the
/// *start directory*, not the command — use e.g.
///   lrmux new-session -- find /
///   lrmux new-session find /
fn parse_new_session(args: &[String]) -> CliAction {
    let parsed = cmd::parse_flags(args);
    let name = parsed
        .get("s")
        .or_else(|| parsed.get("session"))
        .map(|s| s.to_string());
    let cwd = parsed
        .get("c")
        .or_else(|| parsed.get("cwd"))
        .map(|s| s.to_string());
    let detached = cmd::session_detached(&parsed);
    let command = shell_command_from_parsed(&parsed);
    // Common mistake: `new-session -c 'find /'` (shell -c muscle memory).
    // In tmux/lrmux, -c is the start directory.
    if let Some(ref dir) = cwd
        && command.is_none()
        && looks_like_shell_command_not_cwd(dir)
    {
        eprintln!(
            "lrmux: warning: -c is the start directory, not the command.\n\
             To run a command in a new session, use:\n  \
             lrmux new-session -- {dir}\n  \
             lrmux new-session {dir}"
        );
    }
    CliAction::NewSession {
        name,
        cwd,
        command,
        detached,
    }
}

/// True when a `-c` value looks like the user meant a shell command
/// (bash/zsh `-c` habit) rather than a start directory.
fn looks_like_shell_command_not_cwd(value: &str) -> bool {
    // Warn when -c is not an existing directory and looks command-like.
    !std::path::Path::new(value).is_dir()
        && (value.contains(' ')
            || value.contains('|')
            || value.contains(';')
            || value.contains('&')
            || value.contains('>')
            || value.contains('<'))
}

/// Shell command from `-- …` (preferred) or leftover positional args.
fn shell_command_from_parsed(parsed: &cmd::ParsedCmd) -> Option<String> {
    if !parsed.after_dash.is_empty() {
        Some(parsed.after_dash.join(" "))
    } else if !parsed.positional.is_empty() {
        Some(parsed.positional.join(" "))
    } else {
        None
    }
}

/// Parse `attach` / `attach-session`.
///   attach [-s server|host:port] [-t target] [host:port] [session]
///   attach -CC [-s server]
fn parse_attach(args: &[String]) -> CliAction {
    let parsed = cmd::parse_flags(args);
    if parsed.has("CC") {
        let target = parsed
            .get("s")
            .or_else(|| parsed.get("server"))
            .or_else(|| parsed.get("t"))
            .or_else(|| parsed.get("target"))
            .map(|s| s.to_string());
        if let Some(ref t) = target {
            note_tcp_server(t);
        }
        return CliAction::ControlMode { target };
    }
    let server_flag = parsed.get("s").or_else(|| parsed.get("server"));
    let target_flag = parsed.get("t").or_else(|| parsed.get("target"));
    let first_pos = parsed.positional.first().map(|s| s.as_str());
    let second_pos = parsed.positional.get(1).map(|s| s.as_str());
    match cmd::classify_attach(server_flag, target_flag, first_pos, second_pos, |name| {
        ipc::server_exists(&socket_path(name))
    }) {
        cmd::AttachTarget::Tcp { addr, session } => CliAction::AttachSession {
            server: None,
            session,
            tcp: Some(addr),
        },
        cmd::AttachTarget::Local { server, session } => CliAction::AttachSession {
            server,
            session,
            tcp: None,
        },
    }
}

/// If `name` is a TCP endpoint, remember it as the client connect address.
fn note_tcp_server(name: &str) {
    if cmd::is_tcp_connect_target(name, |n| ipc::server_exists(&socket_path(n))) {
        crate::ipc::set_tcp_addr(Some(name.to_string()));
    }
}

/// Parse `new-server` / `start-server` args.
///   new-server [-s name] [--tcp addr] [--ws addr] [--headless] [-CC] [--] [shell-command...]
/// `force_headless` is set for the `start-server` alias.
fn parse_new_server(args: &[String], force_headless: bool) -> CliAction {
    let parsed = cmd::parse_flags(args);
    let session = parsed
        .get("s")
        .or_else(|| parsed.get("server"))
        .map(|s| s.to_string());
    let name = session.clone().unwrap_or_else(ipc::auto_server_name);
    CliAction::NewServer {
        name,
        session,
        control: parsed.has("CC"),
        command: shell_command_from_parsed(&parsed),
        headless: force_headless || parsed.has("headless"),
        tcp: parsed.get("tcp").map(|s| s.to_string()),
        ws: parsed.get("ws").map(|s| s.to_string()),
    }
}

/// Parse `new-window` args using tmux-style flags.
///   new-window [-t target] [-n name] [-c cwd] [--] [shell-command...]
fn parse_new_window(args: &[String]) -> CliAction {
    let parsed = cmd::parse_flags(args);
    let name = parsed
        .get("n")
        .or_else(|| parsed.get("name"))
        .map(|s| s.to_string());
    let cwd = parsed
        .get("c")
        .or_else(|| parsed.get("cwd"))
        .map(|s| s.to_string());
    let command = shell_command_from_parsed(&parsed);
    CliAction::NewWindow {
        target: parsed.target(),
        name,
        cwd,
        command,
    }
}

/// Parse `send-keys` args using tmux-style flags.
///   send-keys -t <target> <keys> -q
fn parse_send_keys(args: &[String]) -> CliAction {
    let parsed = cmd::parse_flags(args);
    let quiet = parsed.has("q") || parsed.has("quiet");
    let keys = parse_tmux_keys(&parsed.positional);
    CliAction::SendKeys {
        target: parsed.target(),
        keys,
        quiet,
    }
}

/// Parse tmux-style key names into byte sequences.
/// Supported:
///   C-a, C-c, C-z  → Ctrl+key (0x01, 0x03, 0x1a)
///   F1-F12         → function key escape sequences
///   Enter          → \r
///   Tab            → \t
///   Escape, Esc    → \x1b
///   Space          → ' '
///   BS, BSpace     → \x7f
///   Up/Down/Left/Right → arrow key escape sequences
///   Home/End       → Home/End escape sequences
///   PageUp/PageDown   → page up/down escape sequences
///   Any other string  → its raw bytes (literal text)
fn parse_tmux_keys(parts: &[String]) -> Vec<u8> {
    let mut result = Vec::new();
    for part in parts {
        match part.as_str() {
            "Enter" | "Return" => result.push(b'\r'),
            "Tab" => result.push(b'\t'),
            "Escape" | "Esc" => result.push(0x1b),
            "Space" => result.push(b' '),
            "BS" | "BSpace" => result.push(0x7f),
            "Up" => result.extend_from_slice(b"\x1b[A"),
            "Down" => result.extend_from_slice(b"\x1b[B"),
            "Right" => result.extend_from_slice(b"\x1b[C"),
            "Left" => result.extend_from_slice(b"\x1b[D"),
            "Home" => result.extend_from_slice(b"\x1b[H"),
            "End" => result.extend_from_slice(b"\x1b[F"),
            "PageUp" | "PgUp" => result.extend_from_slice(b"\x1b[5~"),
            "PageDown" | "PgDn" => result.extend_from_slice(b"\x1b[6~"),
            "F1" => result.extend_from_slice(b"\x1bOP"),
            "F2" => result.extend_from_slice(b"\x1bOQ"),
            "F3" => result.extend_from_slice(b"\x1bOR"),
            "F4" => result.extend_from_slice(b"\x1bOS"),
            "F5" => result.extend_from_slice(b"\x1b[15~"),
            "F6" => result.extend_from_slice(b"\x1b[17~"),
            "F7" => result.extend_from_slice(b"\x1b[18~"),
            "F8" => result.extend_from_slice(b"\x1b[19~"),
            "F9" => result.extend_from_slice(b"\x1b[20~"),
            "F10" => result.extend_from_slice(b"\x1b[21~"),
            "F11" => result.extend_from_slice(b"\x1b[23~"),
            "F12" => result.extend_from_slice(b"\x1b[24~"),
            _ if part.starts_with("C-") && part.len() == 3 => {
                let key = part.as_bytes()[2];
                // C-@ through C-_ → 0x00 through 0x1f
                if key.is_ascii_uppercase() {
                    result.push(key - b'A' + 1);
                } else if key.is_ascii_lowercase() {
                    result.push(key - b'a' + 1);
                } else if key == b'@' {
                    result.push(0x00);
                } else if key == b'[' {
                    result.push(0x1b);
                } else if key == b'\\' {
                    result.push(0x1c);
                } else if key == b']' {
                    result.push(0x1d);
                } else if key == b'^' {
                    result.push(0x1e);
                } else if key == b'_' {
                    result.push(0x1f);
                } else if key == b'?' {
                    result.push(0x7f);
                } else {
                    // Unknown C-x, pass literally.
                    result.extend_from_slice(part.as_bytes());
                }
            }
            _ if part.starts_with("M-") && part.len() == 3 => {
                // M-x → ESC + x (meta prefix)
                result.push(0x1b);
                result.push(part.as_bytes()[2]);
            }
            _ => {
                // Literal text — pass as raw bytes.
                result.extend_from_slice(part.as_bytes());
            }
        }
    }
    result
}

/// Entry point: parse args, connect to or fork a server, run the client.
fn run() -> io::Result<()> {
    // Load config early so [network] applies to server fork and clients.
    let _ = crate::config::global();
    let action = parse_args();

    // Detect nested lrmux — running lrmux inside lrmux hangs because
    // the inner client competes for the same terminal/PTY.
    // Instead of erroring, redirect to non-interactive commands:
    //   lrmux           → create a new window (like Ctrl-A c)
    //   lrmux new-session → create a new session (like Ctrl-A C)
    //   lrmux new-server  → still blocked (can't start a new server from inside)
    //   lrmux ss          → still blocked (needs interactive TTY)
    let nested = std::env::var("LRMUX").is_ok();
    let nested_server = std::env::var("LRMUX_SERVER").unwrap_or_else(|_| "default".to_string());
    if nested {
        match &action {
            CliAction::AttachSession {
                tcp: Some(addr), ..
            } => {
                eprintln!(
                    "lrmux: cannot attach to {addr} from inside lrmux; \
                     detach first (Ctrl-A d)"
                );
                return Ok(());
            }
            CliAction::AttachSession {
                server: Some(srv), ..
            } if srv != &nested_server => {
                eprintln!(
                    "lrmux: cannot attach to server '{srv}' from inside lrmux; \
                     detach first (Ctrl-A d)"
                );
                return Ok(());
            }
            CliAction::AttachSession { session: None, .. } => {
                // `attach` with no session from inside a pane: do nothing.
                // Never invent a new window — that used to happen for typos
                // that fell through to Default, and also for bare `attach`.
                eprintln!(
                    "lrmux: already inside a session on server '{nested_server}'.\n\
                     Use `lrmux new-window` / Ctrl-A c for a window, \
                     or `lrmux new-session` / Ctrl-A C for a session."
                );
                return Ok(());
            }
            CliAction::AttachSession {
                session: Some(sess),
                ..
            } => {
                // Can't attach interactively from inside — the outer client
                // owns the terminal. A session switch on the nested server is ok.
                if sess.is_empty() {
                    eprintln!("lrmux: already attached to server '{nested_server}'");
                    return Ok(());
                }
                // Validate the session exists before claiming a switch.
                if let Ok((_, sessions)) = query_session_names(&nested_server)
                    && !sessions.iter().any(|n| n == sess)
                {
                    eprintln!("lrmux: no session '{sess}' on server '{nested_server}'");
                    return Ok(());
                }
                let sock = socket_path(&nested_server);
                let mut stream = ipc::connect(&sock)?;
                let msg = proto::encode_client(&ClientMsg::Identify {
                    rows: 24,
                    cols: 80,
                    attach: false,
                    auth_token: crate::config::effective_psk(),
                });
                proto::send(&mut stream, &msg)?;
                match proto::decode_server(&mut stream) {
                    Ok(ServerMsg::IdentifyAck { .. }) => {}
                    _ => {
                        eprintln!("lrmux: failed to connect to server");
                        return Ok(());
                    }
                }
                let msg = proto::encode_client(&ClientMsg::SelectSession { name: sess.clone() });
                proto::send(&mut stream, &msg)?;
                std::thread::sleep(std::time::Duration::from_millis(100));
                eprintln!("lrmux: switched to session '{sess}'");
                return Ok(());
            }
            _ => {}
        }
        match action {
            CliAction::Default | CliAction::RunCommand(_) => {
                // Create a new window on the parent's server, non-interactive.
                // RunCommand passes a command to run in the new window.
                // Only these (plus explicit new-window / new-session below)
                // may spawn — never unknown commands.
                let command = match &action {
                    CliAction::RunCommand(cmd) => Some(cmd.clone()),
                    _ => None,
                };
                let sock = socket_path(&nested_server);
                if !ipc::server_exists(&sock) {
                    eprintln!("lrmux: no server running; start one from outside lrmux");
                    return Ok(());
                }
                let mut stream = ipc::connect(&sock)?;
                let msg = proto::encode_client(&ClientMsg::Identify {
                    rows: 24,
                    cols: 80,
                    attach: false,
                    auth_token: crate::config::effective_psk(),
                });
                proto::send(&mut stream, &msg)?;
                match proto::decode_server(&mut stream) {
                    Ok(ServerMsg::IdentifyAck { .. }) => {}
                    _ => {
                        eprintln!("lrmux: failed to connect to server");
                        return Ok(());
                    }
                }
                let msg = proto::encode_client(&ClientMsg::NewWindowIn {
                    session: None,
                    command,
                });
                proto::send(&mut stream, &msg)?;
                // Give the server time to process the message before closing.
                std::thread::sleep(std::time::Duration::from_millis(100));
                eprintln!("lrmux: new window created (use Ctrl-A n/p to switch)");
                return Ok(());
            }
            CliAction::NewSession {
                name,
                cwd,
                command,
                detached: _,
            } => {
                // Create a new session on the parent's server, non-interactive.
                let sock = socket_path(&nested_server);
                if !ipc::server_exists(&sock) {
                    eprintln!("lrmux: no server running; start one from outside lrmux");
                    return Ok(());
                }
                let mut stream = ipc::connect(&sock)?;
                let msg = proto::encode_client(&ClientMsg::Identify {
                    rows: 24,
                    cols: 80,
                    attach: false,
                    auth_token: crate::config::effective_psk(),
                });
                proto::send(&mut stream, &msg)?;
                match proto::decode_server(&mut stream) {
                    Ok(ServerMsg::IdentifyAck { .. }) => {}
                    _ => {
                        eprintln!("lrmux: failed to connect to server");
                        return Ok(());
                    }
                }
                let cwd = cwd.or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                });
                let msg = proto::encode_client(&ClientMsg::NewSession {
                    name: name.clone(),
                    cwd,
                    command,
                });
                proto::send(&mut stream, &msg)?;
                // Send SelectSession so the server switches the interactive client.
                if let Some(ref name) = name {
                    let msg =
                        proto::encode_client(&ClientMsg::SelectSession { name: name.clone() });
                    proto::send(&mut stream, &msg)?;
                }
                // Give the server time to process the messages before closing.
                std::thread::sleep(std::time::Duration::from_millis(100));
                eprintln!("lrmux: new session created (use Ctrl-A N/P to switch)");
                return Ok(());
            }
            CliAction::SelectSession(name) => {
                // Switch the interactive client to a different session.
                let sock = socket_path(&nested_server);
                if !ipc::server_exists(&sock) {
                    eprintln!("lrmux: no server running; start one from outside lrmux");
                    return Ok(());
                }
                let mut stream = ipc::connect(&sock)?;
                let msg = proto::encode_client(&ClientMsg::Identify {
                    rows: 24,
                    cols: 80,
                    attach: false,
                    auth_token: crate::config::effective_psk(),
                });
                proto::send(&mut stream, &msg)?;
                match proto::decode_server(&mut stream) {
                    Ok(ServerMsg::IdentifyAck { .. }) => {}
                    _ => {
                        eprintln!("lrmux: failed to connect to server");
                        return Ok(());
                    }
                }
                let msg = proto::encode_client(&ClientMsg::SelectSession { name: name.clone() });
                proto::send(&mut stream, &msg)?;
                std::thread::sleep(std::time::Duration::from_millis(100));
                eprintln!("lrmux: switched to session '{name}'");
                return Ok(());
            }
            CliAction::NewServer {
                name,
                session,
                control,
                command,
                headless,
                tcp,
                ws,
            } => {
                // Starting a server doesn't need a terminal — only the
                // attach does. Fork it and return; it can be attached to
                // from outside lrmux (or via the selector).
                let sock = socket_path(&name);
                if ipc::server_exists(&sock) {
                    eprintln!("lrmux: server '{name}' is already running");
                    return Ok(());
                }
                if control {
                    eprintln!("lrmux: cannot use -CC from inside lrmux; detach first");
                    return Ok(());
                }
                eprintln!("lrmux: starting server '{name}' on {}...", sock.display());
                fork_server(
                    &sock,
                    tcp.as_deref(),
                    ws.as_deref(),
                    headless,
                    ServerInit {
                        command: command.as_deref(),
                        session: session.as_deref(),
                        cwd: None,
                    },
                )?;
                wait_for_server(&sock)?;
                if headless {
                    eprintln!("lrmux: headless server '{name}' ready.");
                } else if command.is_some() {
                    eprintln!(
                        "lrmux: server '{name}' ready with command (attach from outside lrmux)"
                    );
                } else {
                    eprintln!("lrmux: server '{name}' ready (attach from outside lrmux)");
                }
                return Ok(());
            }
            CliAction::SessionSelector => {
                eprintln!(
                    "lrmux: this command needs an interactive terminal.\n\
                     \n\
                     Use prefix commands instead:\n  \
                     Ctrl-A c    New window\n  \
                     Ctrl-A C    New session\n  \
                     Ctrl-A n/p  Next/prev window\n  \
                     Ctrl-A N/P  Next/prev session\n  \
                     Ctrl-A S    Session chooser\n  \
                     Ctrl-A $    Rename session\n  \
                     Ctrl-A d    Detach\n  \
                     Ctrl-A ?    Show keybindings\n\n\
                     Non-interactive subcommands still work:\n  \
                     lrmux list-sessions [-s <server>]\n  \
                     lrmux list-servers\n  \
                     lrmux new-window -t <target> [-- cmd]\n  \
                     lrmux capture-pane -t <target>\n  \
                     lrmux send-keys -t <target> <keys>\n  \
                     lrmux kill-server -s <name>\n  \
                     lrmux --help"
                );
                return Ok(());
            }
            // Non-interactive subcommands pass through.
            _ => {}
        }
    }

    match action {
        CliAction::Help(topic) => print_help(topic.as_deref()),
        CliAction::Unknown(cmd) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unknown command '{cmd}'. Try `lrmux --help`.\n\
                 (Inside a pane, bare `lrmux` opens a new window; typos do not.)"
            ),
        )),
        CliAction::Versions => {
            print_versions();
            Ok(())
        }
        CliAction::ClientVersion => {
            println!("lrmux {}", crate::version::VERSION);
            Ok(())
        }
        CliAction::ControlMode { target } => run_control_mode(target.as_deref()),
        CliAction::NewSession {
            name,
            cwd,
            command,
            detached,
        } => {
            let sock = socket_path("default");
            // No server yet — start one, bootstrapping the first session with
            // the requested name/cwd/command so we don't leave an empty shell
            // session behind.
            let started_fresh = !ipc::server_exists(&sock);
            if started_fresh {
                eprintln!("lrmux: no server running; starting default server...");
                fork_server(
                    &sock,
                    None,
                    None,
                    detached,
                    ServerInit {
                        command: command.as_deref(),
                        session: name.as_deref(),
                        cwd: cwd.as_deref(),
                    },
                )?;
                wait_for_server(&sock)?;
                eprintln!("lrmux: server ready.");
            }
            if detached {
                if !started_fresh {
                    // An existing server creates the session from NewSession.
                    // A fresh --headless server already did, from its bootstrap env.
                    let mut stream = ipc::connect(&sock)?;
                    let msg = proto::encode_client(&ClientMsg::Identify {
                        rows: 24,
                        cols: 80,
                        attach: false,
                        auth_token: crate::config::effective_psk(),
                    });
                    proto::send(&mut stream, &msg)?;
                    match proto::decode_server(&mut stream) {
                        Ok(ServerMsg::IdentifyAck { .. }) => {}
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::ConnectionRefused,
                                "failed to connect to server",
                            ));
                        }
                    }
                    let msg = proto::encode_client(&ClientMsg::NewSession { name, cwd, command });
                    proto::send(&mut stream, &msg)?;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                eprintln!("lrmux: session created (detached)");
                Ok(())
            } else if started_fresh {
                // First Identify creates the bootstrapped session — just attach.
                match client::run(&sock, None, None, None, None) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "server socket is stale; try again",
                    )),
                    Err(e) => Err(e),
                }
            } else {
                match client::run(&sock, Some(name), None, command, cwd) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "server socket is stale; try again",
                    )),
                    Err(e) => Err(e),
                }
            }
        }
        CliAction::AttachSession {
            server,
            session,
            tcp,
        } => {
            if let Some(addr) = tcp {
                crate::ipc::set_tcp_addr(Some(addr.clone()));
            }
            // The parser already resolved [server][:][session] or host:port.
            let via_tcp = crate::ipc::tcp_addr().is_some();
            let label = crate::ipc::tcp_addr()
                .or(server.clone())
                .unwrap_or_else(|| "default".to_string());
            let server = server.unwrap_or_else(|| "default".to_string());
            let sock = socket_path(&server);
            if !via_tcp && !ipc::server_exists(&sock) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "server '{label}' not running; start it with `lrmux new-server -s {label}`"
                    ),
                ));
            }
            // Validate the requested session exists before attaching —
            // otherwise SelectSession would fail silently and we'd land
            // on an arbitrary session. A TCP failure stays a TCP error:
            // never fall back to forking a local server.
            let (_, sessions) = query_session_names(&server).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("server '{label}' is not responding ({e})"),
                )
            })?;
            if let Some(ref s) = session
                && !sessions.iter().any(|n| n == s)
            {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no session '{s}' on server '{label}'"),
                ));
            }
            // A fresh non-headless server has no sessions yet — create one
            // so the attach isn't a blank screen. Do not invent a session
            // on a remote server that simply has none.
            let new_session = if !via_tcp && sessions.is_empty() && session.is_none() {
                Some(None)
            } else {
                None
            };
            client::run(&sock, new_session, session, None, None)
        }
        CliAction::SelectSession(name) => {
            // Non-interactive: send SelectSession to the default server.
            let sock = socket_path("default");
            if !ipc::server_exists(&sock) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no server running"));
            }
            let mut stream = ipc::connect(&sock)?;
            let msg = proto::encode_client(&ClientMsg::Identify {
                rows: 24,
                cols: 80,
                attach: false,
                auth_token: crate::config::effective_psk(),
            });
            proto::send(&mut stream, &msg)?;
            match proto::decode_server(&mut stream) {
                Ok(ServerMsg::IdentifyAck { .. }) => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "failed to connect to server",
                    ));
                }
            }
            let msg = proto::encode_client(&ClientMsg::SelectSession { name });
            proto::send(&mut stream, &msg)?;
            std::thread::sleep(std::time::Duration::from_millis(100));
            eprintln!("lrmux: switched to session");
            Ok(())
        }
        CliAction::KillSession(_name) => {
            // TODO: implement KillSession protocol message
            eprintln!("lrmux: kill-session not yet implemented (use kill-server)");
            Ok(())
        }
        CliAction::RunCommand(cmd) => {
            // Outside lrmux: start a new server if needed, create a window with the command, attach.
            let sock = socket_path("default");
            if !ipc::server_exists(&sock) {
                return start_new_server("default", Some(None), None, None, None, None);
            }
            // Server exists: create a new window with the command and attach.
            client::run(&sock, None, None, Some(cmd), None)
        }
        CliAction::NewServer {
            name,
            session,
            control,
            command,
            headless,
            tcp,
            ws,
        } => {
            if control {
                let sock = socket_path(&name);
                if ipc::server_exists(&sock) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("server '{name}' is already running"),
                    ));
                }
                unsafe {
                    let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                    if devnull >= 0 {
                        libc::dup2(devnull, libc::STDERR_FILENO);
                        libc::close(devnull);
                    }
                }
                start_headless_server(
                    &name,
                    tcp.as_deref(),
                    ws.as_deref(),
                    ServerInit {
                        command: command.as_deref(),
                        session: session.as_deref(),
                        cwd: None,
                    },
                )?;
                crate::client::control::run(&sock)
            } else if headless {
                start_headless_server(
                    &name,
                    tcp.as_deref(),
                    ws.as_deref(),
                    ServerInit {
                        command: command.as_deref(),
                        session: session.as_deref(),
                        cwd: None,
                    },
                )
            } else {
                start_new_server(
                    &name,
                    None,
                    command,
                    tcp.as_deref(),
                    ws.as_deref(),
                    session.as_deref(),
                )
            }
        }
        CliAction::Discover => cmd_discover(),
        CliAction::Psk(args) => cmd_psk(&args),
        CliAction::Default => run_default(),
        CliAction::SessionSelector => run_session_selector(),
        CliAction::ListServers => list_servers(),
        CliAction::ListSessions(server) => list_sessions(server.as_deref()),
        CliAction::ListWindows(_target) => {
            // TODO: implement list-windows
            eprintln!("lrmux: list-windows not yet implemented");
            Ok(())
        }
        CliAction::KillServer(name) => kill_server(&name),
        CliAction::NewWindow {
            target,
            name: _,
            cwd: _,
            command,
        } => cli_new_window(&target, command),
        CliAction::KillWindow(_target) => {
            eprintln!("lrmux: kill-window not yet implemented");
            Ok(())
        }
        CliAction::SelectWindow(_target) => {
            eprintln!("lrmux: select-window not yet implemented");
            Ok(())
        }
        CliAction::RenameWindow { target: _, name: _ } => {
            eprintln!("lrmux: rename-window not yet implemented");
            Ok(())
        }
        CliAction::CapturePane {
            target,
            print,
            colors,
            format,
            clipboard,
            file,
        } => cli_capture_window(&target, format, colors, print, clipboard, file.as_deref()),
        CliAction::SendKeys {
            target,
            keys,
            quiet,
        } => cli_send_keys(&target, &keys, quiet),
    }
}

/// Default action: show the selector, then act on the user's choice.
fn run_default() -> io::Result<()> {
    dispatch_selector(client::selector::run_selector())
}

/// Force the interactive session selector (no auto-join even if only one session exists).
fn run_session_selector() -> io::Result<()> {
    dispatch_selector(client::selector::run_selector_forced())
}

fn dispatch_selector(result: io::Result<SelectorResult>) -> io::Result<()> {
    match result {
        Ok(SelectorResult::Attach {
            server,
            session,
            tcp,
        }) => {
            if let Some(addr) = tcp {
                crate::ipc::set_tcp_addr(Some(addr));
            }
            let sock = socket_path(&server);
            client::run(&sock, None, Some(session), None, None)
        }
        Ok(SelectorResult::NewSession { server, name, tcp }) => {
            if let Some(addr) = tcp {
                // Remote server: create the session over TCP. Never fork a
                // local server just because the Unix socket is absent here.
                crate::ipc::set_tcp_addr(Some(addr));
                let sock = socket_path(&server);
                return client::run(&sock, Some(name), None, None, None);
            }
            let sock = socket_path(&server);
            if !ipc::server_exists(&sock) {
                start_new_server(&server, None, None, None, None, None)?;
            }
            client::run(&sock, Some(name), None, None, None)
        }
        Ok(SelectorResult::NewServer { name }) => {
            start_new_server(&name, None, None, None, None, Some(&name))
        }
        Ok(SelectorResult::Quit) => Ok(()),
        Err(e) => {
            // Selector failed (e.g. no raw mode) — fall back to default server.
            eprintln!("lrmux: selector unavailable ({e}), starting default server...");
            let sock = socket_path("default");
            if !ipc::server_exists(&sock) {
                fork_server(&sock, None, None, false, ServerInit::default())?;
                wait_for_server(&sock)?;
                eprintln!("lrmux: server ready.");
            }
            client::run(&sock, None, None, None, None)
        }
    }
}

/// Start a new named server and optionally create a session / run a command,
/// then attach.
fn start_new_server(
    name: &str,
    new_session: Option<Option<String>>,
    command: Option<String>,
    tcp_listen: Option<&str>,
    ws_listen: Option<&str>,
    session: Option<&str>,
) -> io::Result<()> {
    let sock = socket_path(name);

    // Check if a server with this name already exists.
    if ipc::server_exists(&sock) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("server '{name}' is already running"),
        ));
    }

    eprintln!("lrmux: starting server '{name}' on {}...", sock.display());
    if let Some(addr) = tcp_listen {
        eprintln!("lrmux: TCP listener: {addr}");
    }
    if let Some(addr) = ws_listen {
        eprintln!("lrmux: WebSocket listener: {addr}");
    }
    // Bootstrap the first session with the command (if any) so we don't get
    // an empty shell session plus a second session for the app.
    // No pre-removal of the socket file: ipc::listen() only unlinks genuinely
    // stale sockets, and wait_for_server() waits for a real connection.
    // `--tcp` here is the listen address. The client attaches on the Unix socket.
    fork_server(
        &sock,
        tcp_listen,
        ws_listen,
        false,
        ServerInit {
            command: command.as_deref(),
            session,
            cwd: None,
        },
    )?;
    wait_for_server(&sock)?;
    eprintln!("lrmux: server '{name}' ready.");

    // Attach only — the first Identify creates the (already-commanded) session.
    // `new_session` is only used when the caller explicitly wants a *second*
    // session on a server that already has one; here the server is fresh.
    let _ = new_session;
    client::run(&sock, None, None, None, None)
}

/// Start a headless server (no TTY needed). Used for testing and remote management.
/// The server creates a default session (24x80) and waits for clients.
fn start_headless_server(
    name: &str,
    tcp_addr: Option<&str>,
    ws_addr: Option<&str>,
    init: ServerInit<'_>,
) -> io::Result<()> {
    let sock = socket_path(name);

    if ipc::server_exists(&sock) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("server '{name}' is already running"),
        ));
    }

    eprintln!(
        "lrmux: starting headless server '{name}' on {}...",
        sock.display()
    );
    if let Some(ref addr) = tcp_addr {
        eprintln!("lrmux: TCP listener: {addr}");
    }
    if let Some(ref addr) = ws_addr {
        eprintln!("lrmux: WebSocket listener: {addr}");
        eprintln!("lrmux: open web/ via a static server, connect to ws://{addr}");
    }
    fork_server(&sock, tcp_addr, ws_addr, true, init)?;
    wait_for_server(&sock)?;
    eprintln!("lrmux: headless server '{name}' ready.");
    eprintln!("lrmux: connect with `lrmux` or use CLI commands (send-keys, capture-window, etc.)");
    Ok(())
}

/// Run iTerm2 control mode against a target server.
/// Default target means the default server (auto-start a headless one if missing).
fn run_control_mode(target: Option<&str>) -> io::Result<()> {
    // Resolve the target and validate the server before silencing stderr,
    // so that real errors (unknown server) are still visible to the user.
    let server = resolve_control_target(target)?;
    let sock = socket_path(&server);
    let via_tcp = crate::ipc::tcp_addr().is_some();
    if !via_tcp && !ipc::server_exists(&sock) {
        if server == "default" {
            // Start a headless default server silently.
            fork_server(&sock, None, None, true, ServerInit::default())?;
            wait_for_server(&sock)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "server '{server}' not running; start it with `lrmux new-server -s {server}`"
                ),
            ));
        }
    }

    // CRITICAL: No output to stdout/stderr except DCS + % notifications.
    // iTerm2 parses everything on the PTY as tmux control protocol.
    // Redirect stderr to /dev/null so server startup messages don't leak.
    unsafe {
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        if devnull >= 0 {
            libc::dup2(devnull, libc::STDERR_FILENO);
            libc::close(devnull);
        }
    }

    crate::client::control::run(&sock)
}

/// Resolve a -t target string to a server name for control mode.
/// `foo` → `foo` if it is a running server, otherwise error.
/// `foo:bar` or `foo:` → `foo`; `:bar` or empty → `default`.
fn resolve_control_target(target: Option<&str>) -> io::Result<String> {
    let t = target.unwrap_or("default");
    if cmd::is_tcp_connect_target(t, |n| ipc::server_exists(&socket_path(n))) {
        crate::ipc::set_tcp_addr(Some(t.to_string()));
        return Ok(t.to_string());
    }
    let (srv, _) = t.split_once(':').unwrap_or((t, ""));
    let srv = if srv.is_empty() { "default" } else { srv };
    Ok(srv.to_string())
}

/// List all running servers (local sockets + LAN discovery).
fn list_servers() -> io::Result<()> {
    let entries = client::inventory::collect(Duration::from_millis(500))?;
    if entries.is_empty() {
        eprintln!("(no servers running)");
        return Ok(());
    }
    for e in entries {
        println!("{}", e.display_label());
    }
    Ok(())
}

/// Broadcast a UDP Discover probe and print LAN servers (shared inventory).
fn cmd_discover() -> io::Result<()> {
    let port = crate::config::global().network.discovery_port;
    eprintln!("lrmux: probing UDP {port}...");
    let entries = client::inventory::collect(Duration::from_millis(800))?;
    let lan: Vec<_> = entries.into_iter().filter(|e| e.is_lan()).collect();
    if lan.is_empty() {
        eprintln!("(no servers answered)");
        return Ok(());
    }
    for e in lan {
        println!("{}", e.display_label());
        for s in &e.sessions {
            println!("  {s}");
        }
    }
    Ok(())
}

/// Query a server's session names and address via a lightweight connection.
/// Times out after 2s — a wedged server accepts but never responds.
fn query_session_names(server: &str) -> io::Result<(String, Vec<String>)> {
    let mut stream = connect_to_server(server)?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let msg = proto::encode_client(&ClientMsg::ListSessions);
    proto::send(&mut stream, &msg)?;
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::SessionList { sessions, address }) => Ok((address, sessions)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected SessionList",
        )),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            Err(io::Error::new(e.kind(), "timed out"))
        }
        Err(e) => Err(e),
    }
}

/// List sessions. With a server argument, list that server's sessions as
/// `session @ <address>`. Without one, list sessions across all running
/// servers (local + LAN) using the shared inventory.
fn list_sessions(server: Option<&str>) -> io::Result<()> {
    if let Some(name) = server {
        let (address, sessions) = query_session_names(name)?;
        if sessions.is_empty() {
            eprintln!("(no sessions)");
        } else {
            for s in &sessions {
                println!("{s} @ {address}");
            }
        }
        return Ok(());
    }

    // With --tcp, query that single endpoint once.
    if crate::ipc::tcp_addr().is_some() {
        let (address, sessions) = query_session_names("tcp")?;
        if sessions.is_empty() {
            eprintln!("(no sessions)");
        } else {
            for s in &sessions {
                println!("{s} @ {address}");
            }
        }
        return Ok(());
    }

    let entries = client::inventory::collect(Duration::from_millis(500))?;
    let multi = entries.len() > 1;
    let mut any = false;
    for e in &entries {
        for s in &e.sessions {
            any = true;
            let tag = if e.is_lan() { "lan" } else { "local" };
            if multi {
                println!("{}:{} @ {} [{tag}]", e.name, s, e.address);
            } else {
                println!("{s} @ {} [{tag}]", e.address);
            }
        }
        if e.sessions.is_empty() {
            any = true;
            let tag = if e.is_lan() { "lan" } else { "local" };
            println!("{} @ {} [{tag}] (no sessions)", e.name, e.address);
        }
    }
    if !any {
        eprintln!("(no sessions)");
    }
    Ok(())
}

/// CLI: show / set / generate the PSK used for TCP auth.
fn cmd_psk(args: &[String]) -> io::Result<()> {
    match args.first().map(|s| s.as_str()) {
        None | Some("show") => {
            let p = crate::config::effective_psk();
            if p.is_empty() {
                println!("(no PSK configured)");
            } else {
                let preview = if p.len() > 8 {
                    format!("{}... ({} chars)", &p[..4], p.len())
                } else {
                    "********".to_string()
                };
                println!("psk: {preview}");
                println!("tip: use `lrmux psk generate` to create a new one");
            }
            Ok(())
        }
        Some("set") => {
            let psk = args.get(1).cloned().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "usage: lrmux psk set <secret>")
            })?;
            crate::config::persist_psk(&psk)?;
            let _ = push_psk_to_running_server(&psk);
            println!("psk saved to {}", crate::config::config_path().display());
            println!("remote clients: lrmux --tcp <host:port> --psk '<secret>'");
            Ok(())
        }
        Some("generate") | Some("gen") => {
            let psk = crate::config::generate_psk()?;
            crate::config::persist_psk(&psk)?;
            let _ = push_psk_to_running_server(&psk);
            println!("{psk}");
            eprintln!(
                "lrmux: PSK saved to {}. Share this string with remote clients — do not copy config files.",
                crate::config::config_path().display()
            );
            eprintln!("lrmux: ensure the server listens on TCP (network.tcp_listen or --tcp).");
            eprintln!("lrmux: with default safe_networks=[], TLS is required for all TCP peers.");
            Ok(())
        }
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown psk subcommand '{other}' (show|set|generate)"),
        )),
    }
}

fn push_psk_to_running_server(psk: &str) -> io::Result<()> {
    let sock = socket_path("default");
    if !ipc::server_exists(&sock) {
        return Ok(());
    }
    let mut stream = ipc::connect(&sock).map(ipc::ConnStream::Unix)?;
    let msg = proto::encode_client(&ClientMsg::Identify {
        rows: 24,
        cols: 80,
        attach: true,
        auth_token: crate::config::effective_psk(),
    });
    proto::send(&mut stream, &msg)?;
    let _ = proto::decode_server(&mut stream)?;
    let msg = proto::encode_client(&ClientMsg::SetPsk {
        psk: psk.to_string(),
    });
    proto::send(&mut stream, &msg)?;
    let _ = proto::decode_server(&mut stream)?;
    Ok(())
}

/// Whether to emit ANSI color in CLI output.
fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// Print the client version and the versions of all running servers.
fn print_versions() {
    let color = use_color();
    let reset = if color { "\x1b[0m" } else { "" };
    let cyan = if color { "\x1b[36m" } else { "" };
    let green = if color { "\x1b[32m" } else { "" };
    let yellow = if color { "\x1b[33m" } else { "" };
    let red = if color { "\x1b[31m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };

    println!("{cyan}client{reset}: {}", crate::version::VERSION);

    let uid = unsafe { libc::getuid() };
    let dir = format!("/tmp/lrmux-{uid}");
    let mut servers: Vec<String> = Vec::new();
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
    if servers.is_empty() {
        eprintln!("{dim}(no servers running){reset}");
        return;
    }

    println!();
    println!("{green}servers found{reset}");
    println!("{dim}-------------{reset}");

    for name in &servers {
        match query_server_version(name) {
            Ok((version, address)) => {
                println!("{yellow}{name}{reset} @ {cyan}{address}{reset}: {version}")
            }
            Err(e) => eprintln!("{red}lrmux:{reset} server '{name}' is not responding ({e})"),
        }
    }
}

/// Query a running server for its version string and address.
fn query_server_version(server: &str) -> io::Result<(String, String)> {
    let mut stream = connect_to_server(server)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let identify = proto::encode_client(&ClientMsg::Identify {
        rows: 0,
        cols: 0,
        attach: false,
        auth_token: crate::config::effective_psk(),
    });
    proto::send(&mut stream, &identify)?;
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::IdentifyAck {
            version, address, ..
        }) => Ok((version, address)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected IdentifyAck",
        )),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            Err(io::Error::new(e.kind(), "timed out"))
        }
        Err(e) => Err(e),
    }
}

/// Kill a named server by sending a KillServer message.
/// Falls back to removing the socket file if the server can't be reached.
fn kill_server(name: &str) -> io::Result<()> {
    // Try TCP first if --tcp is set.
    if crate::ipc::tcp_addr().is_some() {
        let mut stream = connect_to_server(name)?;
        let msg = proto::encode_client(&ClientMsg::KillServer);
        proto::send(&mut stream, &msg)?;
        eprintln!("lrmux: sent KillServer via TCP.");
        std::thread::sleep(Duration::from_millis(100));
        eprintln!("lrmux: killed server '{name}'.");
        return Ok(());
    }

    let sock = socket_path(name);
    if !sock.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("server '{name}' socket not found"),
        ));
    }

    // Try to connect and send KillServer message.
    match ipc::connect(&sock) {
        Ok(mut stream) => {
            let msg = proto::encode_client(&ClientMsg::KillServer);
            if proto::send(&mut stream, &msg).is_ok() {
                eprintln!("lrmux: sent KillServer to '{name}'.");
                // Give the server a moment to clean up.
                std::thread::sleep(Duration::from_millis(100));
            }
            // Also remove the socket file in case the server didn't clean up.
            let _ = std::fs::remove_file(&sock);
            eprintln!("lrmux: killed server '{name}'.");
            Ok(())
        }
        Err(_) => {
            // Server is not responding — just remove the stale socket file.
            std::fs::remove_file(&sock)?;
            eprintln!("lrmux: removed stale socket for '{name}'.");
            Ok(())
        }
    }
}

/// CLI: create a new window in a session.
fn cli_new_window(target: &cmd::Target, command: Option<String>) -> io::Result<()> {
    let mut stream = connect_to_server("default")?;
    // Send Identify first (required by the protocol).
    let (rows, cols) = (24u16, 80u16);
    let msg = proto::encode_client(&ClientMsg::Identify {
        rows,
        cols,
        attach: false,
        auth_token: crate::config::effective_psk(),
    });
    proto::send(&mut stream, &msg)?;
    // Wait for IdentifyAck.
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::IdentifyAck { .. }) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected IdentifyAck",
            ));
        }
        Err(e) => return Err(e),
    }
    // Send NewWindowIn.
    let msg = proto::encode_client(&ClientMsg::NewWindowIn {
        session: target.session.clone(),
        command,
    });
    proto::send(&mut stream, &msg)?;
    Ok(())
}

/// CLI: capture the content of a pane.
///
/// Output destinations:
/// - stdout when there is no `--file`/`--clipboard`, or when `-p`/`--print` is set
/// - `--file <path>` writes the capture to that file
/// - `--clipboard` copies via pbcopy / wl-copy / xclip / xsel
fn cli_capture_window(
    target: &cmd::Target,
    format: crate::server::CaptureFormat,
    colors: bool,
    print: bool,
    clipboard: bool,
    file: Option<&str>,
) -> io::Result<()> {
    let mut stream = connect_to_server("default")?;
    let (rows, cols) = (24u16, 80u16);
    let msg = proto::encode_client(&ClientMsg::Identify {
        rows,
        cols,
        attach: false,
        auth_token: crate::config::effective_psk(),
    });
    proto::send(&mut stream, &msg)?;
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::IdentifyAck { .. }) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected IdentifyAck",
            ));
        }
        Err(e) => return Err(e),
    }
    let window = target.window.as_ref().and_then(|w| w.parse::<u8>().ok());
    let msg = proto::encode_client(&ClientMsg::CaptureWindow {
        session: target.session.clone(),
        window,
        format: format.as_u8(),
        colors,
        // Palette comes from the pane (OSC 10/11 answered for that PTY),
        // not from this CLI's TTY — they can differ.
        term_fg: None,
        term_bg: None,
    });
    proto::send(&mut stream, &msg)?;
    // Wait for WindowCapture response.
    loop {
        match proto::decode_server(&mut stream) {
            Ok(ServerMsg::WindowCapture { content }) => {
                let to_stdout = print || (file.is_none() && !clipboard);
                if to_stdout {
                    print!("{content}");
                    if !content.ends_with('\n') {
                        println!();
                    }
                }
                if let Some(path) = file {
                    std::fs::write(path, &content)?;
                }
                if clipboard && !client::copy_mode::copy_to_clipboard(&content) {
                    return Err(io::Error::other(
                        "clipboard: no pbcopy/wl-copy/xclip/xsel found",
                    ));
                }
                return Ok(());
            }
            Ok(_) => {
                // Ignore other messages (StatusBarUpdate, etc.) and keep waiting.
            }
            Err(e) => return Err(e),
        }
    }
}

/// CLI: send keys to a pane's PTY.
fn cli_send_keys(target: &cmd::Target, keys: &[u8], quiet: bool) -> io::Result<()> {
    let mut stream = connect_to_server("default")?;
    let (rows, cols) = (24u16, 80u16);
    let msg = proto::encode_client(&ClientMsg::Identify {
        rows,
        cols,
        attach: false,
        auth_token: crate::config::effective_psk(),
    });
    proto::send(&mut stream, &msg)?;
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::IdentifyAck { .. }) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected IdentifyAck",
            ));
        }
        Err(e) => return Err(e),
    }
    if !quiet {
        eprintln!(
            "lrmux: send-keys: target={:?} keys={:?} ({} bytes)",
            target,
            String::from_utf8_lossy(keys),
            keys.len()
        );
    }
    let window = target.window.as_ref().and_then(|w| w.parse::<u8>().ok());
    let msg = proto::encode_client(&ClientMsg::SendKeys {
        session: target.session.clone(),
        window,
        keys: keys.to_vec(),
    });
    proto::send(&mut stream, &msg)?;
    // Give the server time to process the message before we close the socket.
    std::thread::sleep(std::time::Duration::from_millis(100));
    if !quiet {
        eprintln!("lrmux: send-keys: message sent");
    }
    Ok(())
}

/// Bootstrap for a freshly forked server: first session/window options.
#[derive(Default)]
struct ServerInit<'a> {
    command: Option<&'a str>,
    session: Option<&'a str>,
    cwd: Option<&'a str>,
}

/// Fork a server process. The child binds the socket and runs the event loop.
fn fork_server(
    sock: &Path,
    tcp_addr: Option<&str>,
    ws_addr: Option<&str>,
    headless: bool,
    init: ServerInit<'_>,
) -> io::Result<()> {
    // CWD for the first session: explicit `-c`, else the client's current
    // directory (where this `lrmux` process was invoked).
    let cwd_owned = init.cwd.map(|s| s.to_string()).or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    });

    // Pass bootstrap options to the child via env (cleared after fork in
    // both parent and by the server's take_bootstrap()).
    // Safety: single-threaded around fork.
    unsafe {
        match init.command {
            Some(cmd) => std::env::set_var("LRMUX_INIT_COMMAND", cmd),
            None => std::env::remove_var("LRMUX_INIT_COMMAND"),
        }
        match init.session {
            Some(name) => std::env::set_var("LRMUX_INIT_SESSION", name),
            None => std::env::remove_var("LRMUX_INIT_SESSION"),
        }
        match cwd_owned.as_deref() {
            Some(cwd) => std::env::set_var("LRMUX_INIT_CWD", cwd),
            None => std::env::remove_var("LRMUX_INIT_CWD"),
        }
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // Child process — become the server.
        // Inherits the client's TERM/COLORTERM as-is (never rewritten).
        unsafe {
            libc::setsid();
        }
        let sock = sock.to_path_buf();
        let tcp = tcp_addr.map(|s| s.to_string());
        let ws = ws_addr.map(|s| s.to_string());
        match server::run(&sock, tcp.as_deref(), ws.as_deref(), headless) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("lrmux server: {e}");
                std::process::exit(1);
            }
        }
    }
    // Parent — drop init env so it doesn't leak into later client work.
    unsafe {
        std::env::remove_var("LRMUX_INIT_COMMAND");
        std::env::remove_var("LRMUX_INIT_SESSION");
        std::env::remove_var("LRMUX_INIT_CWD");
    }
    Ok(())
}

/// Wait for the server to bind the socket (retry loop).
/// Just checks for socket file existence — the kernel will queue
/// the client's connect() until the server calls accept().
fn wait_for_server(sock: &Path) -> io::Result<()> {
    // Poll until the socket accepts connections — a stale socket file that
    // merely exists must not satisfy this wait (connect on it fails with
    // ECONNREFUSED until the new server rebinds).
    for _ in 0..500 {
        if ipc::server_exists(sock) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "server did not start in time",
    ))
}
