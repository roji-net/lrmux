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
}

/// Parse CLI arguments into an action.
fn parse_args() -> CliAction {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("new-session") => CliAction::NewSession(args.get(2).cloned()),
        Some("new-server") => {
            let name = args.get(2).map(|s| s.as_str()).unwrap_or("default");
            CliAction::NewServer(name.to_string())
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
        _ => CliAction::Default,
    }
}

/// Entry point: parse args, connect to or fork a server, run the client.
fn run() -> io::Result<()> {
    let action = parse_args();

    match action {
        CliAction::NewSession(name) => {
            // Connect to the default server (must already be running).
            let sock = socket_path("default");
            match client::run(&sock, Some(name)) {
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
        CliAction::LsServers => list_servers(),
        CliAction::LsSessions(server) => list_sessions(&server),
        CliAction::KillServer(name) => kill_server(&name),
    }
}

/// Default action: show the selector, then act on the user's choice.
fn run_default() -> io::Result<()> {
    match client::selector::run_selector() {
        Ok(SelectorResult::Attach { server, session: _ }) => {
            // Attach to the selected server (session 0 for now; full session
            // selection will be added when the protocol supports it).
            let sock = socket_path(&server);
            client::run(&sock, None)
        }
        Ok(SelectorResult::NewSession { server, name }) => {
            let sock = socket_path(&server);
            if !ipc::server_exists(&sock) {
                // Server doesn't exist — start it first.
                start_new_server(&server, None)?;
            }
            client::run(&sock, Some(name))
        }
        Ok(SelectorResult::NewServer { name }) => start_new_server(&name, None),
        Ok(SelectorResult::Quit) => Ok(()),
        Err(e) => {
            // Selector failed (e.g. no raw mode) — fall back to default server.
            eprintln!("lrmux: selector unavailable ({e}), starting default server...");
            let sock = socket_path("default");
            if !ipc::server_exists(&sock) {
                fork_server(&sock)?;
                wait_for_server(&sock)?;
                eprintln!("lrmux: server ready.");
            }
            client::run(&sock, None)
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
    fork_server(&sock)?;
    wait_for_server(&sock)?;
    eprintln!("lrmux: server '{name}' ready.");

    client::run(&sock, new_session)
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

/// Kill a named server by removing its socket (the server will detect and exit).
fn kill_server(name: &str) -> io::Result<()> {
    let sock = socket_path(name);
    if !sock.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("server '{name}' socket not found"),
        ));
    }
    // Removing the socket file will cause the server's accept() to fail,
    // and it will exit. A more graceful approach would send a KillServer
    // message, but this works for now.
    std::fs::remove_file(&sock)?;
    eprintln!("lrmux: killed server '{name}'.");
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
