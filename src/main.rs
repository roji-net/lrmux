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

use crate::ipc::socket_path;

fn main() {
    if let Err(e) = run() {
        eprintln!("lrmux: {e}");
        std::process::exit(1);
    }
}

/// CLI arguments.
enum CliAction {
    /// Default: connect to the "default" server (or start one).
    Default,
    /// `new-session [name]`: connect to default server, create a new session, attach to it.
    NewSession(Option<String>),
    /// `new-server [name]`: start a new server with the given name (or "default").
    NewServer(String),
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
        CliAction::NewServer(name) => {
            // Start a new server with the given name.
            let sock = socket_path(&name);

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

            // Connect as client (no new session — the server starts with one).
            client::run(&sock, None)
        }
        CliAction::Default => {
            let sock = socket_path("default");

            // Try to connect to an existing server.
            match client::run(&sock, None) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // Socket file doesn't exist — fork a new server.
                }
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    // Stale socket — clean up and fork a new server.
                    ipc::cleanup(&sock);
                }
                Err(e) => return Err(e),
            }

            // Fork a server process.
            eprintln!("lrmux: starting server on {}...", sock.display());
            fork_server(&sock)?;

            // Wait for the server to bind the socket.
            wait_for_server(&sock)?;
            eprintln!("lrmux: server ready.");

            // Connect as client.
            client::run(&sock, None)
        }
    }
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
