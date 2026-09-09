// Server event loop: poll over PTY master and client socket.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};
use crate::server::pane::Pane;

/// Run the server event loop.
///
/// Phase 3: single session, single window, single pane.
/// Accepts one client connection, relays I/O, and exits when the client
/// disconnects or the child process exits.
pub fn run(listener: UnixListener, socket_path: &std::path::Path) -> io::Result<()> {
    listener.set_nonblocking(true)?;

    // Accept one client connection.
    let (mut client, _addr) = accept_client(&listener)?;
    let client_fd = client.as_raw_fd();

    // Read the Identify message to get terminal size.
    let (rows, cols) = match proto::decode_client(&mut client) {
        Ok(ClientMsg::Identify { rows, cols }) => (rows, cols),
        _ => {
            let _ = proto::send(
                &mut client,
                &proto::encode_server(&ServerMsg::Error {
                    msg: "expected Identify".into(),
                }),
            );
            return Ok(());
        }
    };

    // Spawn the pane (PTY + grid).
    let mut pane = Pane::new(rows, cols);
    let pty_fd = pane.pty_fd();

    // Send IdentifyAck + full grid snapshot.
    let ack = proto::encode_server(&ServerMsg::IdentifyAck { rows, cols });
    proto::send(&mut client, &ack)?;

    // Mark all rows dirty so the first GridUpdate sends the full screen.
    pane.grid.mark_all_dirty();
    send_grid_update(&mut client, &mut pane)?;

    // Event loop: poll PTY master + client socket.
    let mut child_alive = true;
    let mut client_buf: Vec<u8> = Vec::new();

    loop {
        let mut fds = [
            libc::pollfd {
                fd: pty_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: client_fd,
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

        // PTY output → grid → client
        if fds[0].revents & libc::POLLIN != 0 && child_alive {
            match pane.process_pty_output() {
                Ok(true) => {
                    send_grid_update(&mut client, &mut pane)?;
                }
                Ok(false) => {
                    child_alive = false;
                    let _ = proto::send(
                        &mut client,
                        &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                    );
                }
                Err(e) => {
                    child_alive = false;
                    let _ = proto::send(
                        &mut client,
                        &proto::encode_server(&ServerMsg::Error {
                            msg: format!("pty read error: {e}"),
                        }),
                    );
                }
            }
        }

        // Client input → parse frames → PTY
        if fds[1].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 8192];
            let n = unsafe { libc::read(client_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                client_buf.extend_from_slice(&buf[..n as usize]);
                // Parse all complete frames from the buffer.
                while let Some(msg) = try_parse_frame(&mut client_buf)? {
                    match msg {
                        ClientMsg::PaneInput { data } => {
                            pane.write_input(&data)?;
                        }
                        ClientMsg::Resize { rows, cols } => {
                            pane.resize(rows, cols);
                            pane.grid.mark_all_dirty();
                            send_grid_update(&mut client, &mut pane)?;
                        }
                        ClientMsg::Detach => {
                            break;
                        }
                        ClientMsg::Identify { .. } => {
                            // Ignore — already handled.
                        }
                    }
                }
            } else if n == 0 {
                break;
            } else {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::WouldBlock {
                    break;
                }
            }
        }

        // Check for hangup / error.
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 && !child_alive {
            break;
        }
        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
    }

    // Cleanup.
    ipc::cleanup(socket_path);
    Ok(())
}

/// Accept a client connection (blocking).
/// Returns the stream in blocking mode (for the initial handshake).
fn accept_client(
    listener: &UnixListener,
) -> io::Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::SocketAddr,
)> {
    listener.set_nonblocking(false)?;
    let stream = listener.accept()?;
    // Keep the stream blocking for the handshake; switch to non-blocking later.
    Ok(stream)
}

/// Try to parse a complete frame from the buffer.
/// Returns None if the buffer doesn't contain a complete message yet.
/// Consumes the frame from the buffer if successful.
fn try_parse_frame(buf: &mut Vec<u8>) -> io::Result<Option<ClientMsg>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return Ok(None);
    }
    // We have a complete frame. Extract it.
    let frame: Vec<u8> = buf.drain(..4 + len).collect();
    // Decode the type + payload directly.
    let payload = &frame[4..];
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty frame payload",
        ));
    }
    let msg_type = payload[0];
    let data = &payload[1..];
    let msg = match msg_type {
        0x01 => {
            if data.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Identify needs 4 bytes",
                ));
            }
            let rows = u16::from_le_bytes([data[0], data[1]]);
            let cols = u16::from_le_bytes([data[2], data[3]]);
            ClientMsg::Identify { rows, cols }
        }
        0x02 => ClientMsg::PaneInput {
            data: data.to_vec(),
        },
        0x03 => {
            if data.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Resize needs 4 bytes",
                ));
            }
            let rows = u16::from_le_bytes([data[0], data[1]]);
            let cols = u16::from_le_bytes([data[2], data[3]]);
            ClientMsg::Resize { rows, cols }
        }
        0x04 => ClientMsg::Detach,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown client msg type: {msg_type}"),
            ));
        }
    };
    Ok(Some(msg))
}

/// Send a GridUpdate message with dirty rows + cursor position.
fn send_grid_update<W: Write>(writer: &mut W, pane: &mut Pane) -> io::Result<()> {
    let dirty = pane.take_dirty_rows();
    if dirty.is_empty() {
        return Ok(());
    }
    let (cursor_row, cursor_col, cursor_visible) = pane.cursor();
    let msg = proto::encode_server(&ServerMsg::GridUpdate {
        dirty,
        cursor_row,
        cursor_col,
        cursor_visible,
    });
    proto::send(writer, &msg)
}
