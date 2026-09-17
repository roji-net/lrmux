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
use std::sync::Mutex;
use std::time::Duration;

use crate::client::selector::SelectorResult;
use crate::ipc::socket_path;
use crate::proto::{ClientMsg, ServerMsg};

/// Global TCP address for CLI commands (--tcp <addr>).
/// When set, CLI commands connect via TCP instead of Unix socket.
static TCP_ADDR: Mutex<Option<String>> = Mutex::new(None);

/// Connect to a server. Uses TCP if --tcp was set, otherwise Unix socket.
/// `LRMUX_CLI_SERVER` overrides the default server name for CLI helpers
/// (capture-pane, send-keys, …) so tests can target a throwaway server
/// without touching the user's `default`.
fn connect_to_server(server: &str) -> io::Result<crate::ipc::ConnStream> {
    if let Some(addr) = TCP_ADDR.lock().unwrap().as_ref() {
        return crate::ipc::connect_tcp(addr);
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
    /// `attach-session [-s <server>] [{-t <target> | <target>}]`
    /// target: [server][:][session].
    AttachSession {
        server: Option<String>,
        session: Option<String>,
    },
    /// `select-session -t <name>`: switch the interactive client to a session.
    SelectSession(String),
    /// `kill-session -t <name>`: kill a session.
    KillSession(Option<String>),
    /// `new-server -s <name> [-CC] [--] [<cmd> [args...]]`: start a new server,
    /// optionally in iTerm2 control mode, optionally running a command in the
    /// first session (same syntax as `new-session`).
    NewServer {
        name: String,
        control: bool,
        command: Option<String>,
    },
    /// `start-server -s <name> [--tcp addr]`: start a headless server.
    StartServer { name: String, tcp: Option<String> },
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
    /// `capture-pane -t <target> -p`: capture the content of a pane.
    CapturePane { target: cmd::Target, print: bool },
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
    /// `--help` / `-h`: show usage.
    Help,
    /// Unrecognized subcommand — must not fall through to Default
    /// (inside a pane, Default creates a new window).
    Unknown(String),
}

/// Print usage information.
fn print_help() {
    println!(
        "lrmux — a modern, fast terminal multiplexer\n\
         \n\
         USAGE:\n    \
         lrmux [COMMAND] [OPTIONS] [ARGS]\n\
         \n\
         COMMANDS (tmux-compatible syntax):\n    \
         lrmux                       Attach to a session (selector if multiple exist)\n    \
         lrmux -- <cmd> [args]       Create a new window running <cmd> and attach\n    \
         lrmux new-session [-s <name>] [-c <cwd>] [-d] [--] [<cmd> [args...]]\n    \
         lrmux attach-session [-s <server>] ([-t <[server:][session]>] | <[server:][session]>)\n    \
         lrmux select-session -t <name>\n    \
         lrmux kill-session -t <name>\n    \
         lrmux new-window -t <target> -n <name> [-c <cwd>] [--] [<cmd> [args...]]\n    \
         lrmux kill-window -t <target>\n    \
         lrmux select-window -t <target>\n    \
         lrmux rename-window -t <target> <name>\n    \
         lrmux send-keys -t <target> <keys> [-q]\n    \
         lrmux capture-pane -t <target> [-p]\n    \
         lrmux list-sessions [-s <server>]\n    \
         lrmux list-windows -t <session>\n    \
         lrmux list-servers\n    \
         lrmux kill-server -s <name>\n    \
         lrmux new-server [-s <name>] [-CC] [--] [<cmd> [args...]]\n    \
         lrmux start-server -s <name> [--tcp <addr>]\n    \
         lrmux session-selector      Force the interactive session selector\n    \
         lrmux versions              Show client and all running server versions
    \
         lrmux -CC [-s <server>]     tmux control mode (for iTerm2 integration)
    \
         lrmux attach -CC -s <server>  Attach to a server in iTerm2 control mode\n    \
         lrmux --help, -h            Show this help message\n\
         \n\
         ALIASES:\n    \
         new=new-session  ls=list-sessions  lsw=list-windows\n    \
         neww=new-window  send=send-keys  capturep=capture-pane\n    \
         ss=session-selector\n\
         \n\
         TARGETS (-t):\n    \
         <session>             A session by name\n    \
         <session>:<window>    A window by index in a session\n    \
         :<window>             Window in current session\n    \
         (empty)               Current session/window\n\
         \n\
         SEND-KEYS:\n    \
         Special keys: C-c, C-a, F1-F12, Enter, Tab, Escape, Space, BS,\n    \
         Up, Down, Left, Right, Home, End, PageUp, PageDown, M-x\n    \
         Literal text is passed as-is.\n    \
         -q / --quiet: suppress stderr output\n\
         \n\
         NESTED USAGE:\n    \
         Running lrmux inside lrmux creates a new window (like Ctrl-A c).\n    \
         Running `lrmux new-session` inside lrmux creates a new session.\n    \
         `lrmux new-server` and `lrmux ss` need an interactive terminal.\n\
         \n\
         ENVIRONMENT:\n    \
         LRMUX_SYSLOG=host:port   Send logs to remote syslog (UDP RFC 3164)\n    \
         LRMUX_LOG_LEVEL=debug|info|warn|error   Log level (default: info)\n    \
         LRMUX=1                  Set automatically inside lrmux panes\n    \
         LRMUX_SERVER=name        Server name (set automatically inside lrmux panes)\n\
         \n\
         PREFIX KEY: Ctrl-A (default)\n\
         \n\
         COMMON PREFIX COMMANDS:\n    \
         Ctrl-A c    New window\n    \
         Ctrl-A n/p  Next/prev window\n    \
         Ctrl-A Ctrl-A  Toggle last window\n    \
         Ctrl-A 0-9  Select window\n    \
         Ctrl-A C    New session\n    \
         Ctrl-A N/P  Next/prev session\n    \
         Ctrl-A S    Session chooser\n    \
         Ctrl-A $    Rename session\n    \
         Ctrl-A [    Enter copy mode\n    \
         Ctrl-A ]    Paste\n    \
         Ctrl-A d    Detach\n    \
         Ctrl-A x    Kill pane\n    \
         Ctrl-A F    Resize to terminal\n    \
         Ctrl-A K    Kill session\n    \
         Ctrl-A \\    Show server log\n    \
         Ctrl-A ?    Show keybindings\n\
         \n\
         LOGS:\n    \
         File: /tmp/lrmux-<UID>/logs/<server>.log\n    \
         State: /tmp/lrmux-<UID>/logs/<server>.state\n    \
         Ring log: Ctrl-A \\ (in-session, last 500 entries)\n\
         \n\
         TCP:\n    \
         lrmux start-server --tcp <addr>  Listen on TCP\n    \
         lrmux --tcp <addr> <command>      Connect via TCP"
    );
}

/// Parse CLI arguments into an action.
fn parse_args() -> CliAction {
    let args: Vec<String> = std::env::args().collect();
    // Check if the subcommand is `start-server` — if so, don't extract --tcp
    // globally because `start-server` has its own --tcp parser.
    let is_start_server = args.iter().any(|a| a == "start-server");
    // Extract --tcp <addr> flag if present (global, for CLI commands).
    let mut tcp_addr: Option<String> = None;
    let mut filtered: Vec<String> = vec![args[0].clone()];
    let mut i = 1;
    while i < args.len() {
        if !is_start_server && args[i] == "--tcp" && i + 1 < args.len() {
            tcp_addr = Some(args[i + 1].clone());
            i += 2;
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }
    let args = filtered;
    // Store tcp_addr in a global for CLI commands to use.
    if let Some(ref addr) = tcp_addr {
        *TCP_ADDR.lock().unwrap() = Some(addr.clone());
    }

    let subcmd = args.get(1).map(|s| s.as_str());
    let subcmd_args = if args.len() > 2 { &args[2..] } else { &[] };

    // `-h`/`--help` after any subcommand shows the general help
    // (except after `--`, where args belong to the wrapped command).
    if subcmd != Some("--") && subcmd_args.iter().any(|a| a == "-h" || a == "--help") {
        return CliAction::Help;
    }

    match subcmd {
        Some("--help") | Some("-h") => CliAction::Help,
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
                CliAction::Help
            } else {
                CliAction::RunCommand(cmd)
            }
        }
        Some("session-selector") | Some("ss") => CliAction::SessionSelector,
        Some("new-session") | Some("new") => parse_new_session(subcmd_args),
        Some("attach-session") | Some("attach") => {
            let parsed = cmd::parse_flags(subcmd_args);
            if parsed.has("CC") {
                CliAction::ControlMode {
                    target: parsed
                        .get("s")
                        .or_else(|| parsed.get("server"))
                        .or_else(|| parsed.get("t"))
                        .or_else(|| parsed.get("target"))
                        .map(|s| s.to_string()),
                }
            } else {
                // -s / --server sets the server explicitly.
                // -t / --target or the first positional arg is [server:][session].
                let server = parsed.get("s").or_else(|| parsed.get("server"));
                let target = parsed
                    .get("t")
                    .or_else(|| parsed.get("target"))
                    .or_else(|| parsed.positional.first().map(|s| s.as_str()));

                let (srv, sess) = if let Some(srv) = server {
                    // -s means server only; any other value is a session name.
                    (Some(srv.to_string()), target.map(|t| t.to_string()))
                } else if let Some(t) = target {
                    if let Some((srv, sess)) = t.split_once(':') {
                        (
                            if srv.is_empty() {
                                None
                            } else {
                                Some(srv.to_string())
                            },
                            if sess.is_empty() {
                                None
                            } else {
                                Some(sess.to_string())
                            },
                        )
                    } else if ipc::server_exists(&socket_path(t)) {
                        (Some(t.to_string()), None)
                    } else {
                        (None, Some(t.to_string()))
                    }
                } else {
                    (None, None)
                };

                CliAction::AttachSession {
                    server: srv,
                    session: sess,
                }
            }
        }
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
        Some("new-server") => parse_new_server(subcmd_args),
        Some("start-server") => parse_start_server(subcmd_args),
        Some("list-servers") | Some("ls-servers") => CliAction::ListServers,
        Some("list-sessions") | Some("ls-sessions") | Some("ls") => {
            let parsed = cmd::parse_flags(subcmd_args);
            CliAction::ListSessions(
                parsed
                    .get("s")
                    .or_else(|| parsed.get("server"))
                    .map(|s| s.to_string())
                    .or_else(|| parsed.positional.first().cloned()),
            )
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
                .unwrap_or_else(|| "default".to_string());
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
            CliAction::CapturePane {
                target: parsed.target(),
                print: parsed.has("p") || parsed.has("print"),
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
    let detached = parsed.has("d") || parsed.has("detach");
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

/// Parse `new-server` args.
///   new-server [-s name] [-CC] [--] [shell-command...]
fn parse_new_server(args: &[String]) -> CliAction {
    let parsed = cmd::parse_flags(args);
    let name = parsed
        .get("s")
        .or_else(|| parsed.get("server"))
        .map(|s| s.to_string())
        .unwrap_or_else(ipc::auto_server_name);
    CliAction::NewServer {
        name,
        control: parsed.has("CC"),
        command: shell_command_from_parsed(&parsed),
    }
}

/// Parse `start-server` args: optional name + optional --tcp addr.
///   start-server -s <name> --tcp <addr>
fn parse_start_server(args: &[String]) -> CliAction {
    let parsed = cmd::parse_flags(args);
    let name = parsed
        .get("s")
        .or_else(|| parsed.get("server"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "default".to_string());
    let tcp = parsed.get("tcp").map(|s| s.to_string());
    CliAction::StartServer { name, tcp }
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
                control,
                command,
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
                    None,
                    false,
                    ServerInit {
                        command: command.as_deref(),
                        session: None,
                        cwd: None,
                    },
                )?;
                wait_for_server(&sock)?;
                if command.is_some() {
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
        CliAction::Help => {
            print_help();
            Ok(())
        }
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
                    false,
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
                if started_fresh {
                    // Non-headless server creates the session on first Identify.
                    let mut stream = ipc::connect(&sock)?;
                    let msg = proto::encode_client(&ClientMsg::Identify {
                        rows: 24,
                        cols: 80,
                        attach: false,
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
                } else {
                    let mut stream = ipc::connect(&sock)?;
                    let msg = proto::encode_client(&ClientMsg::Identify {
                        rows: 24,
                        cols: 80,
                        attach: false,
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
        CliAction::AttachSession { server, session } => {
            // The parser already resolved [server][:][session].
            let server = server.unwrap_or_else(|| "default".to_string());
            let sock = socket_path(&server);
            if !ipc::server_exists(&sock) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "server '{server}' not running; start it with `lrmux new-server -s {server}`"
                    ),
                ));
            }
            // Validate the requested session exists before attaching —
            // otherwise SelectSession would fail silently and we'd land
            // on an arbitrary session.
            let (_, sessions) = query_session_names(&server).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("server '{server}' is not responding ({e})"),
                )
            })?;
            if let Some(ref s) = session
                && !sessions.iter().any(|n| n == s)
            {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no session '{s}' on server '{server}'"),
                ));
            }
            // A fresh non-headless server has no sessions yet — create one
            // so the attach isn't a blank screen.
            let new_session = if sessions.is_empty() && session.is_none() {
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
                return start_new_server("default", Some(None), None);
            }
            // Server exists: create a new window with the command and attach.
            client::run(&sock, None, None, Some(cmd), None)
        }
        CliAction::NewServer {
            name,
            control,
            command,
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
                    None,
                    ServerInit {
                        command: command.as_deref(),
                        session: None,
                        cwd: None,
                    },
                )?;
                crate::client::control::run(&sock)
            } else {
                start_new_server(&name, None, command)
            }
        }
        CliAction::StartServer { name, tcp } => {
            start_headless_server(&name, tcp.as_deref(), ServerInit::default())
        }
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
        CliAction::CapturePane { target, print: _ } => cli_capture_window(&target),
        CliAction::SendKeys {
            target,
            keys,
            quiet,
        } => cli_send_keys(&target, &keys, quiet),
    }
}

/// Default action: show the selector, then act on the user's choice.
fn run_default() -> io::Result<()> {
    match client::selector::run_selector() {
        Ok(SelectorResult::Attach { server, session }) => {
            // Attach to the selected server and switch to the selected session.
            let sock = socket_path(&server);
            client::run(&sock, None, Some(session), None, None)
        }
        Ok(SelectorResult::NewSession { server, name }) => {
            let sock = socket_path(&server);
            if !ipc::server_exists(&sock) {
                // Server doesn't exist — start it first.
                start_new_server(&server, None, None)?;
            }
            client::run(&sock, Some(name), None, None, None)
        }
        Ok(SelectorResult::NewServer { name }) => start_new_server(&name, None, None),
        Ok(SelectorResult::Quit) => Ok(()),
        Err(e) => {
            // Selector failed (e.g. no raw mode) — fall back to default server.
            eprintln!("lrmux: selector unavailable ({e}), starting default server...");
            let sock = socket_path("default");
            if !ipc::server_exists(&sock) {
                fork_server(&sock, None, false, ServerInit::default())?;
                wait_for_server(&sock)?;
                eprintln!("lrmux: server ready.");
            }
            client::run(&sock, None, None, None, None)
        }
    }
}

/// Force the interactive session selector (no auto-join even if only one session exists).
fn run_session_selector() -> io::Result<()> {
    match client::selector::run_selector_forced() {
        Ok(SelectorResult::Attach { server, session }) => {
            let sock = socket_path(&server);
            client::run(&sock, None, Some(session), None, None)
        }
        Ok(SelectorResult::NewSession { server, name }) => {
            let sock = socket_path(&server);
            if !ipc::server_exists(&sock) {
                start_new_server(&server, None, None)?;
            }
            client::run(&sock, Some(name), None, None, None)
        }
        Ok(SelectorResult::NewServer { name }) => start_new_server(&name, None, None),
        Ok(SelectorResult::Quit) => Ok(()),
        Err(e) => {
            eprintln!("lrmux: selector unavailable ({e}), starting default server...");
            let sock = socket_path("default");
            if !ipc::server_exists(&sock) {
                fork_server(&sock, None, false, ServerInit::default())?;
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
    // Bootstrap the first session with the command (if any) so we don't get
    // an empty shell session plus a second session for the app.
    fork_server(
        &sock,
        None,
        false,
        ServerInit {
            command: command.as_deref(),
            session: None,
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
    fork_server(&sock, tcp_addr, true, init)?;
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
    if !ipc::server_exists(&sock) {
        if server == "default" {
            // Start a headless default server silently.
            fork_server(&sock, None, true, ServerInit::default())?;
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
    let (srv, _) = t.split_once(':').unwrap_or((t, ""));
    let srv = if srv.is_empty() { "default" } else { srv };
    Ok(srv.to_string())
}

/// List all running servers.
fn list_servers() -> io::Result<()> {
    let uid = unsafe { libc::getuid() };
    let dir = format!("/tmp/lrmux-{uid}");
    let mut found = false;
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if ipc::server_exists(&path)
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                println!("{name}");
                found = true;
            }
        }
    }
    if !found {
        eprintln!("(no servers running)");
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
/// servers in `server:session @ <address>` format.
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

    // All servers: discover sockets in /tmp/lrmux-<UID>/.
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
    let multi = servers.len() > 1;
    let mut any = false;
    for name in &servers {
        let (address, sessions) = match query_session_names(name) {
            Ok((a, s)) => (a, s),
            Err(e) => {
                eprintln!("lrmux: server '{name}' is not responding ({e})");
                continue;
            }
        };
        for s in &sessions {
            any = true;
            if multi {
                println!("{name}:{s} @ {address}");
            } else {
                println!("{s} @ {address}");
            }
        }
    }
    if !any {
        eprintln!("(no sessions)");
    }
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
    if TCP_ADDR.lock().unwrap().is_some() {
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
fn cli_capture_window(target: &cmd::Target) -> io::Result<()> {
    let mut stream = connect_to_server("default")?;
    let (rows, cols) = (24u16, 80u16);
    let msg = proto::encode_client(&ClientMsg::Identify {
        rows,
        cols,
        attach: false,
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
    });
    proto::send(&mut stream, &msg)?;
    // Wait for WindowCapture response.
    loop {
        match proto::decode_server(&mut stream) {
            Ok(ServerMsg::WindowCapture { content }) => {
                print!("{content}");
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
        match server::run(&sock, tcp.as_deref(), headless) {
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
