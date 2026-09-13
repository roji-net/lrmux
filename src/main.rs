// lrmux — a modern, fast, minimal-dependency terminal multiplexer

// Phase 3: client/server split. The binary tries to connect to an existing
// server; if none exists, it forks a server process and then connects as a client.

#![allow(dead_code)]

mod client;
mod config;
mod grid;
mod ipc;
mod keys;
mod layout;
mod proto;
mod pty;
mod server;
mod statusbar;
mod term;
mod vt;

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::client::selector::SelectorResult;
use crate::ipc::socket_path;
use crate::proto::{ClientMsg, ServerMsg};

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
    /// `session-selector` / `ss`: force the interactive selector (no auto-join).
    SessionSelector,
    /// `new-session [name]`: connect to default server, create a new session, attach to it.
    NewSession(Option<String>),
    /// `new-server [name]`: start a new server with the given name (or "default").
    NewServer(String),
    /// `ls-servers`: list all running servers.
    LsServers,
    /// `ls-sessions [server]`: list sessions on a server (default: "default").
    LsSessions(String),
    /// `kill-server [name]`: kill a named server (default: "default").
    KillServer(String),
    /// `new-window [target] [command]`: create a new window in a session.
    NewWindow {
        server: String,
        session: Option<String>,
        command: Option<String>,
    },
    /// `capture-window [target]`: capture the content of a window.
    CaptureWindow {
        server: String,
        session: Option<String>,
        window: Option<u8>,
    },
    /// `send-keys [target] [keys]`: send keys to a window's PTY.
    SendKeys {
        server: String,
        session: Option<String>,
        window: Option<u8>,
        keys: String,
    },
    /// `--help` / `-h`: show usage.
    Help,
}

/// Print usage information.
fn print_help() {
    println!(
        "lrmux — a modern, fast terminal multiplexer\n\
         \n\
         USAGE:\n    \
         lrmux [COMMAND] [ARGS]\n\
         \n\
         COMMANDS:\n    \
         lrmux                  Attach to a session (selector if multiple exist)\n    \
         lrmux session-selector  Force the interactive session selector\n    \
         lrmux ss               Alias for session-selector\n    \
         lrmux new-session [N]   Create a new session on the default server\n    \
         lrmux new-server [N]    Start a new named server\n    \
         lrmux ls-servers        List running servers\n    \
         lrmux ls-sessions [S]   List sessions on a server (default: default)\n    \
         lrmux kill-server [N]   Kill a named server (default: default)\n    \
         lrmux new-window [T] [CMD]  Create a new window (T = [server]:[session])\n    \
         lrmux capture-window [T]    Capture window content (T = [server]:[session]:window)\n    \
         lrmux send-keys [T] [KEYS]  Send keys to a window (T = [server]:[session]:window)\n    \
         lrmux --help, -h        Show this help message\n\
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
         Ctrl-A $    Rename session\n    \
         Ctrl-A [    Enter copy mode\n    \
         Ctrl-A ]    Paste\n    \
         Ctrl-A d    Detach\n    \
         Ctrl-A x    Kill pane\n    \
         Ctrl-A F    Resize to terminal\n    \
         Ctrl-A K    Kill session"
    );
}

/// Parse CLI arguments into an action.
fn parse_args() -> CliAction {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--help") | Some("-h") => CliAction::Help,
        Some("session-selector") | Some("ss") => CliAction::SessionSelector,
        Some("new-session") => CliAction::NewSession(args.get(2).cloned()),
        Some("new-server") => {
            // Auto-generate a name if none provided (server-2, server-3, ...).
            let name = args
                .get(2)
                .map(|s| s.to_string())
                .unwrap_or_else(ipc::auto_server_name);
            CliAction::NewServer(name)
        }
        Some("ls-servers") => CliAction::LsServers,
        Some("ls-sessions") => {
            let server = args.get(2).map(|s| s.as_str()).unwrap_or("default");
            CliAction::LsSessions(server.to_string())
        }
        Some("kill-server") => {
            let name = args.get(2).map(|s| s.as_str()).unwrap_or("default");
            CliAction::KillServer(name.to_string())
        }
        Some("new-window") => parse_new_window(&args[2..]),
        Some("capture-window") => parse_capture_window(&args[2..]),
        Some("send-keys") => parse_send_keys(&args[2..]),
        _ => CliAction::Default,
    }
}

/// Parsed target: [server]:[session]:window
struct Target {
    server: String,
    session: Option<String>,
    window: Option<u8>,
}

/// Parse a target string like "1", "luar:1", or "default:luar:1".
/// - 1 part → window number (server=default, session=None)
/// - 2 parts → session:window (server=default)
/// - 3 parts → server:session:window
fn parse_target(s: &str) -> Target {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => {
            // Could be a window number or a session name (without window).
            if let Ok(w) = parts[0].parse::<u8>() {
                Target {
                    server: "default".to_string(),
                    session: None,
                    window: Some(w),
                }
            } else {
                Target {
                    server: "default".to_string(),
                    session: Some(parts[0].to_string()),
                    window: None,
                }
            }
        }
        2 => {
            let window = parts[1].parse::<u8>().ok();
            Target {
                server: "default".to_string(),
                session: Some(parts[0].to_string()),
                window,
            }
        }
        3 => {
            let window = parts[2].parse::<u8>().ok();
            Target {
                server: parts[0].to_string(),
                session: Some(parts[1].to_string()),
                window,
            }
        }
        _ => Target {
            server: "default".to_string(),
            session: None,
            window: None,
        },
    }
}

/// Parse `new-window` args: optional target + optional command.
/// Supports --server and --session flags, or positional target.
fn parse_new_window(args: &[String]) -> CliAction {
    let mut server = "default".to_string();
    let mut session: Option<String> = None;
    let mut command_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" if i + 1 < args.len() => {
                server = args[i + 1].clone();
                i += 2;
            }
            "--session" if i + 1 < args.len() => {
                session = Some(args[i + 1].clone());
                i += 2;
            }
            _ => {
                // First positional arg could be a target (contains ':').
                if command_parts.is_empty() && args[i].contains(':') {
                    let t = parse_target(&args[i]);
                    server = t.server;
                    if t.session.is_some() {
                        session = t.session;
                    }
                    i += 1;
                } else {
                    command_parts.push(args[i].clone());
                    i += 1;
                }
            }
        }
    }
    let command = if command_parts.is_empty() {
        None
    } else {
        Some(command_parts.join(" "))
    };
    CliAction::NewWindow {
        server,
        session,
        command,
    }
}

/// Parse `capture-window` args: optional target.
/// Supports --server, --session, --window flags, or positional target.
fn parse_capture_window(args: &[String]) -> CliAction {
    let mut server = "default".to_string();
    let mut session: Option<String> = None;
    let mut window: Option<u8> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" if i + 1 < args.len() => {
                server = args[i + 1].clone();
                i += 2;
            }
            "--session" if i + 1 < args.len() => {
                session = Some(args[i + 1].clone());
                i += 2;
            }
            "--window" if i + 1 < args.len() => {
                window = args[i + 1].parse::<u8>().ok();
                i += 2;
            }
            _ => {
                // Positional target.
                let t = parse_target(&args[i]);
                server = t.server;
                if t.session.is_some() {
                    session = t.session;
                }
                if t.window.is_some() {
                    window = t.window;
                }
                i += 1;
            }
        }
    }
    CliAction::CaptureWindow {
        server,
        session,
        window,
    }
}

/// Parse `send-keys` args: optional target + keys string.
/// Supports --server, --session, --window flags, or positional target.
fn parse_send_keys(args: &[String]) -> CliAction {
    let mut server = "default".to_string();
    let mut session: Option<String> = None;
    let mut window: Option<u8> = None;
    let mut key_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" if i + 1 < args.len() => {
                server = args[i + 1].clone();
                i += 2;
            }
            "--session" if i + 1 < args.len() => {
                session = Some(args[i + 1].clone());
                i += 2;
            }
            "--window" if i + 1 < args.len() => {
                window = args[i + 1].parse::<u8>().ok();
                i += 2;
            }
            _ => {
                // First positional with ':' is a target.
                if key_parts.is_empty() && args[i].contains(':') {
                    let t = parse_target(&args[i]);
                    server = t.server;
                    if t.session.is_some() {
                        session = t.session;
                    }
                    if t.window.is_some() {
                        window = t.window;
                    }
                    i += 1;
                } else {
                    key_parts.push(args[i].clone());
                    i += 1;
                }
            }
        }
    }
    let keys = key_parts.join(" ");
    CliAction::SendKeys {
        server,
        session,
        window,
        keys,
    }
}

/// Entry point: parse args, connect to or fork a server, run the client.
fn run() -> io::Result<()> {
    let action = parse_args();

    // Detect nested lrmux — running lrmux inside lrmux hangs because
    // the inner client competes for the same terminal/PTY.
    // Allow non-interactive subcommands (ls-*, kill-server, help) but
    // block anything that tries to attach (default, new-session, new-server).
    let nested = std::env::var("LRMUX").is_ok();
    let needs_tty = matches!(
        action,
        CliAction::Default
            | CliAction::SessionSelector
            | CliAction::NewSession(_)
            | CliAction::NewServer(_)
    );
    if nested && needs_tty {
        eprintln!(
            "lrmux: cannot attach from inside lrmux.\n\
             \n\
             Use prefix commands instead:\n  \
             Ctrl-A c    New window\n  \
             Ctrl-A C    New session\n  \
             Ctrl-A n/p  Next/prev window\n  \
             Ctrl-A N/P  Next/prev session\n  \
             Ctrl-A $    Rename session\n  \
             Ctrl-A d    Detach\n\n\
             Non-interactive subcommands still work:\n  \
             lrmux ls-sessions\n  \
             lrmux ls-servers\n  \
             lrmux new-window [target] [command]\n  \
             lrmux capture-window [target]\n  \
             lrmux send-keys [target] [keys]\n  \
             lrmux kill-server [name]\n  \
             lrmux --help"
        );
        return Ok(());
    }

    match action {
        CliAction::Help => {
            print_help();
            Ok(())
        }
        CliAction::NewSession(name) => {
            // Connect to the default server (must already be running).
            let sock = socket_path("default");
            match client::run(&sock, Some(name), None) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no server running; start one with `lrmux` first",
                )),
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "server socket is stale; start a new one with `lrmux`",
                )),
                Err(e) => Err(e),
            }
        }
        CliAction::NewServer(name) => start_new_server(&name, None),
        CliAction::Default => run_default(),
        CliAction::SessionSelector => run_session_selector(),
        CliAction::LsServers => list_servers(),
        CliAction::LsSessions(server) => list_sessions(&server),
        CliAction::KillServer(name) => kill_server(&name),
        CliAction::NewWindow {
            server,
            session,
            command,
        } => cli_new_window(&server, session, command),
        CliAction::CaptureWindow {
            server,
            session,
            window,
        } => cli_capture_window(&server, session, window),
        CliAction::SendKeys {
            server,
            session,
            window,
            keys,
        } => cli_send_keys(&server, session, window, &keys),
    }
}

/// Default action: show the selector, then act on the user's choice.
fn run_default() -> io::Result<()> {
    match client::selector::run_selector() {
        Ok(SelectorResult::Attach { server, session }) => {
            // Attach to the selected server and switch to the selected session.
            let sock = socket_path(&server);
            client::run(&sock, None, Some(session))
        }
        Ok(SelectorResult::NewSession { server, name }) => {
            let sock = socket_path(&server);
            if !ipc::server_exists(&sock) {
                // Server doesn't exist — start it first.
                start_new_server(&server, None)?;
            }
            client::run(&sock, Some(name), None)
        }
        Ok(SelectorResult::NewServer { name }) => start_new_server(&name, None),
        Ok(SelectorResult::Quit) => Ok(()),
        Err(e) => {
            // Selector failed (e.g. no raw mode) — fall back to default server.
            eprintln!("lrmux: selector unavailable ({e}), starting default server...");
            let sock = socket_path("default");
            if !ipc::server_exists(&sock) {
                if sock.exists() {
                    let _ = std::fs::remove_file(&sock);
                }
                fork_server(&sock)?;
                wait_for_server(&sock)?;
                eprintln!("lrmux: server ready.");
            }
            client::run(&sock, None, None)
        }
    }
}

/// Force the interactive session selector (no auto-join even if only one session exists).
fn run_session_selector() -> io::Result<()> {
    match client::selector::run_selector_forced() {
        Ok(SelectorResult::Attach { server, session }) => {
            let sock = socket_path(&server);
            client::run(&sock, None, Some(session))
        }
        Ok(SelectorResult::NewSession { server, name }) => {
            let sock = socket_path(&server);
            if !ipc::server_exists(&sock) {
                start_new_server(&server, None)?;
            }
            client::run(&sock, Some(name), None)
        }
        Ok(SelectorResult::NewServer { name }) => start_new_server(&name, None),
        Ok(SelectorResult::Quit) => Ok(()),
        Err(e) => {
            eprintln!("lrmux: selector unavailable ({e}), starting default server...");
            let sock = socket_path("default");
            if !ipc::server_exists(&sock) {
                if sock.exists() {
                    let _ = std::fs::remove_file(&sock);
                }
                fork_server(&sock)?;
                wait_for_server(&sock)?;
                eprintln!("lrmux: server ready.");
            }
            client::run(&sock, None, None)
        }
    }
}

/// Start a new named server and optionally create a session, then attach.
fn start_new_server(name: &str, new_session: Option<Option<String>>) -> io::Result<()> {
    let sock = socket_path(name);

    // Check if a server with this name already exists.
    if ipc::server_exists(&sock) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("server '{name}' is already running"),
        ));
    }

    eprintln!("lrmux: starting server '{name}' on {}...", sock.display());
    // Remove any stale socket file before forking so wait_for_server()
    // doesn't see the old socket and race ahead of the new server.
    if sock.exists() {
        let _ = std::fs::remove_file(&sock);
    }
    fork_server(&sock)?;
    wait_for_server(&sock)?;
    eprintln!("lrmux: server '{name}' ready.");

    client::run(&sock, new_session, None)
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

/// List sessions on a server.
fn list_sessions(server: &str) -> io::Result<()> {
    let sock = socket_path(server);
    if !ipc::server_exists(&sock) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("server '{server}' is not running"),
        ));
    }
    let mut stream = ipc::connect(&sock)?;
    let msg = proto::encode_client(&ClientMsg::ListSessions);
    proto::send(&mut stream, &msg)?;
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::SessionList { sessions }) => {
            if sessions.is_empty() {
                eprintln!("(no sessions)");
            } else {
                for s in &sessions {
                    println!("{s}");
                }
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected SessionList",
        )),
        Err(e) => Err(e),
    }
}

/// Kill a named server by sending a KillServer message.
/// Falls back to removing the socket file if the server can't be reached.
fn kill_server(name: &str) -> io::Result<()> {
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

/// CLI: create a new window in a session on a server.
fn cli_new_window(
    server: &str,
    session: Option<String>,
    command: Option<String>,
) -> io::Result<()> {
    let sock = socket_path(server);
    if !ipc::server_exists(&sock) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("server '{server}' is not running"),
        ));
    }
    let mut stream = ipc::connect(&sock)?;
    // Send Identify first (required by the protocol).
    let (rows, cols) = (24u16, 80u16);
    let msg = proto::encode_client(&ClientMsg::Identify { rows, cols });
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
    let msg = proto::encode_client(&ClientMsg::NewWindowIn { session, command });
    proto::send(&mut stream, &msg)?;
    Ok(())
}

/// CLI: capture the content of a window.
fn cli_capture_window(server: &str, session: Option<String>, window: Option<u8>) -> io::Result<()> {
    let sock = socket_path(server);
    if !ipc::server_exists(&sock) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("server '{server}' is not running"),
        ));
    }
    let mut stream = ipc::connect(&sock)?;
    let (rows, cols) = (24u16, 80u16);
    let msg = proto::encode_client(&ClientMsg::Identify { rows, cols });
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
    let msg = proto::encode_client(&ClientMsg::CaptureWindow { session, window });
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

/// CLI: send keys to a window's PTY.
fn cli_send_keys(
    server: &str,
    session: Option<String>,
    window: Option<u8>,
    keys: &str,
) -> io::Result<()> {
    let sock = socket_path(server);
    if !ipc::server_exists(&sock) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("server '{server}' is not running"),
        ));
    }
    let mut stream = ipc::connect(&sock)?;
    let (rows, cols) = (24u16, 80u16);
    let msg = proto::encode_client(&ClientMsg::Identify { rows, cols });
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
    let msg = proto::encode_client(&ClientMsg::SendKeys {
        session,
        window,
        keys: keys.as_bytes().to_vec(),
    });
    proto::send(&mut stream, &msg)?;
    Ok(())
}

/// Fork a server process. The child binds the socket and runs the event loop.
fn fork_server(sock: &Path) -> io::Result<()> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // Child process — become the server.
        unsafe {
            libc::setsid();
        }
        let sock = sock.to_path_buf();
        match server::run(&sock) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("lrmux server: {e}");
                std::process::exit(1);
            }
        }
    }
    // Parent — return and become the client.
    Ok(())
}

/// Wait for the server to bind the socket (retry loop).
/// Just checks for socket file existence — the kernel will queue
/// the client's connect() until the server calls accept().
fn wait_for_server(sock: &Path) -> io::Result<()> {
    for _ in 0..100 {
        if sock.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "server did not start in time",
    ))
}
