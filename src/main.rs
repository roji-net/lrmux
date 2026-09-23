// lrmux — a modern, fast, minimal-dependency terminal multiplexer

// Phase 0: stubs are not yet wired up. Remove this allow as modules are implemented.
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

use std::io::{self, Write};
use std::os::fd::AsRawFd;

use client::render::Renderer;
use grid::Grid;
use pty::{Pty, PtySize, default_shell_argv};
use vt::parse_bytes;

fn main() {
    if let Err(e) = run_single_shell() {
        eprintln!("lrmux: {e}");
        std::process::exit(1);
    }
}

/// Phase 2: run a single shell in a PTY, parsing output through a VT parser
/// into a grid, then rendering the grid to the terminal via diff-based output.
fn run_single_shell() -> io::Result<()> {
    // Enter raw mode on the controlling terminal.
    let _raw_guard = client::terminal::enter_raw_mode()?;

    // Enter alternate screen so we don't corrupt the caller's terminal.
    {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?1049h");
        let _ = stdout.flush();
    }

    // Get terminal size and spawn a shell.
    let (rows, cols) = client::terminal::get_size();
    let argv = default_shell_argv();
    let pty = Pty::spawn(&argv, PtySize { rows, cols });

    // Create the grid and renderer.
    let mut grid = Grid::new(rows as usize, cols as usize, 10_000);
    let mut renderer = Renderer::new(rows as usize, cols as usize);
    let mut vt_parser = vte::Parser::new();

    // Clear the screen and do an initial render.
    {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[2J\x1b[H");
        let _ = stdout.flush();
    }

    // Relay loop.
    relay_loop(&pty, &mut grid, &mut renderer, &mut vt_parser)?;

    // Restore terminal.
    {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
    }

    Ok(())
}

/// Poll-based relay: stdin → PTY master, PTY master → VT parser → grid → renderer → stdout.
/// Exits when the child process terminates (PTY master returns EOF/error).
fn relay_loop(
    pty: &Pty,
    grid: &mut Grid,
    renderer: &mut Renderer,
    vt_parser: &mut vte::Parser,
) -> io::Result<()> {
    let stdin_fd = io::stdin().as_raw_fd();
    let pty_fd = pty.master_fd();

    let mut buf = [0u8; 8192];

    loop {
        // Build pollfd array: stdin (readable) + PTY master (readable).
        let mut fds = [
            libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: pty_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }

        // stdin → PTY master (passthrough, no interception yet)
        if fds[0].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let n = n as usize;
                let mut written = 0;
                while written < n {
                    let w = unsafe {
                        libc::write(
                            pty_fd,
                            buf[written..].as_ptr() as *const _,
                            (n - written) as _,
                        )
                    };
                    if w < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    written += w as usize;
                }
            }
        }

        // PTY master → VT parser → grid → renderer → stdout
        if fds[1].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(pty_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let n = n as usize;
                // Parse PTY output into the grid.
                parse_bytes(vt_parser, grid, &buf[..n]);
                // Render the grid to stdout (diff-based).
                let mut stdout = io::stdout();
                renderer.render(&mut stdout, grid)?;
            } else if n == 0 {
                break;
            } else {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    continue;
                }
                if err.raw_os_error() == Some(libc::EIO) {
                    break;
                }
                return Err(err);
            }
        }

        // Check for hangup / error on either fd.
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
    }

    Ok(())
}
