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

/// Entry point: try to connect to an existing server, or fork a new one.
fn run() -> io::Result<()> {
    let sock = socket_path("default");

    // Try to connect to an existing server.
    match client::run(&sock) {
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
    fork_server(&sock)?;

    // Wait for the server to bind the socket.
    wait_for_server(&sock)?;

    // Connect as client.
    client::run(&sock)
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
