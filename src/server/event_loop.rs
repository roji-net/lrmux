// Server event loop: poll over listener, PTY master, and all client sockets.
//
// Supports multiple concurrent clients sharing a single pane.
// Grid updates are broadcast to all connected clients.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};
use crate::server::pane::Pane;

/// A connected client.
struct ClientConn {
    stream: UnixStream,
    fd: i32,
    buf: Vec<u8>,
}

impl ClientConn {
    fn new(stream: UnixStream) -> Self {
        let fd = stream.as_raw_fd();
        Self {
            stream,
            fd,
            buf: Vec::new(),
        }
    }
}

/// Run the server event loop.
///
/// Phase 3: single session, single window, single pane, multiple clients.
/// Waits for the first client to determine terminal size, then accepts
/// additional clients concurrently. All clients share the same pane view.
/// The server exits when the child process exits.
pub fn run(listener: UnixListener, socket_path: &std::path::Path) -> io::Result<()> {
    // Wait for the first client to determine terminal size.
    let (first_stream, _) = listener.accept()?;
    let (mut pane, mut clients) = handshake_first_client(first_stream)?;
    let pty_fd = pane.pty_fd();

    // Set listener to non-blocking so we can poll it alongside clients.
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();

    let mut child_alive = true;

    loop {
        // Build pollfd array: listener + PTY + all clients.
        let mut fds = Vec::with_capacity(2 + clients.len());
        fds.push(libc::pollfd {
            fd: listener_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        if child_alive {
            fds.push(libc::pollfd {
                fd: pty_fd,
                events: libc::POLLIN,
                revents: 0,
            });
        } else {
            // Dummy fd (negative) so indices stay aligned — we skip it.
            fds.push(libc::pollfd {
                fd: -1,
                events: 0,
                revents: 0,
            });
        }
        for c in &clients {
            fds.push(libc::pollfd {
                fd: c.fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }

        let ret = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }

        // Capture the number of clients that were polled, so we don't
        // process newly-accepted clients in this iteration (their fds
        // are not in the current pollfd array).
        let polled_clients = clients.len();

        // Listener readable → accept new client.
        if fds[0].revents & libc::POLLIN != 0 {
            accept_new_client(&listener, &pane, &mut clients)?;
        }

        // PTY output → grid → broadcast to all clients.
        if child_alive && fds[1].revents & libc::POLLIN != 0 {
            match pane.process_pty_output() {
                Ok(true) => {
                    broadcast_grid_update(&mut clients, &mut pane)?;
                }
                Ok(false) => {
                    child_alive = false;
                    broadcast_to_all(
                        &mut clients,
                        &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                    );
                }
                Err(e) => {
                    child_alive = false;
                    broadcast_to_all(
                        &mut clients,
                        &proto::encode_server(&ServerMsg::Error {
                            msg: format!("pty read error: {e}"),
                        }),
                    );
                }
            }
        }

        // Client input → parse frames → PTY.
        // Only process clients that were in the pollfd array.
        let mut to_remove: Vec<usize> = Vec::new();
        let mut need_resize_broadcast = false;

        for client_idx in 0..polled_clients {
            let pf = &fds[2 + client_idx];
            if pf.revents & libc::POLLIN != 0 {
                let mut buf = [0u8; 8192];
                let n = unsafe {
                    libc::read(
                        clients[client_idx].fd,
                        buf.as_mut_ptr() as *mut _,
                        buf.len(),
                    )
                };
                if n > 0 {
                    clients[client_idx]
                        .buf
                        .extend_from_slice(&buf[..n as usize]);
                    // Parse all complete frames.
                    loop {
                        match try_parse_frame(&mut clients[client_idx].buf) {
                            Ok(Some(msg)) => match msg {
                                ClientMsg::PaneInput { data } => {
                                    pane.write_input(&data)?;
                                }
                                ClientMsg::Resize { rows, cols } => {
                                    pane.resize(rows, cols);
                                    pane.grid.mark_all_dirty();
                                    need_resize_broadcast = true;
                                }
                                ClientMsg::Detach => {
                                    to_remove.push(client_idx);
                                    break;
                                }
                                ClientMsg::Identify { .. } => {}
                            },
                            Ok(None) => break,
                            Err(_) => {
                                to_remove.push(client_idx);
                                break;
                            }
                        }
                    }
                } else if n == 0 {
                    to_remove.push(client_idx);
                } else {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::WouldBlock {
                        to_remove.push(client_idx);
                    }
                }
            }
            // Check for hangup/error on this client fd.
            if pf.revents & (libc::POLLHUP | libc::POLLERR) != 0 && !to_remove.contains(&client_idx)
            {
                to_remove.push(client_idx);
            }
        }

        // Broadcast resized grid to all clients if a resize happened.
        if need_resize_broadcast {
            broadcast_grid_update(&mut clients, &mut pane)?;
        }

        // Remove disconnected clients (in reverse order to preserve indices).
        for &idx in to_remove.iter().rev() {
            if idx < clients.len() {
                clients.remove(idx);
            }
        }

        // PTY hangup (child exited).
        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 && !child_alive {
            break;
        }

        // If child is dead and all clients are gone, exit.
        if !child_alive && clients.is_empty() {
            break;
        }
    }

    ipc::cleanup(socket_path);
    Ok(())
}

/// Handshake with the first client: read Identify, create pane, send ack + snapshot.
fn handshake_first_client(stream: UnixStream) -> io::Result<(Pane, Vec<ClientConn>)> {
    let mut client = stream;

    // Read the Identify message (blocking).
    let (rows, cols) = match proto::decode_client(&mut client) {
        Ok(ClientMsg::Identify { rows, cols }) => (rows, cols),
        _ => {
            let _ = proto::send(
                &mut client,
                &proto::encode_server(&ServerMsg::Error {
                    msg: "expected Identify".into(),
                }),
            );
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected Identify",
            ));
        }
    };

    // Spawn the pane.
    let mut pane = Pane::new(rows, cols);

    // Send IdentifyAck.
    let ack = proto::encode_server(&ServerMsg::IdentifyAck { rows, cols });
    proto::send(&mut client, &ack)?;

    // Send full grid snapshot (mark all dirty).
    pane.grid.mark_all_dirty();
    send_grid_update(&mut client, &mut pane)?;

    let conn = ClientConn::new(client);
    Ok((pane, vec![conn]))
}

/// Accept a new client, do the handshake, and add it to the clients list.
fn accept_new_client(
    listener: &UnixListener,
    pane: &Pane,
    clients: &mut Vec<ClientConn>,
) -> io::Result<()> {
    match listener.accept() {
        Ok((mut stream, _)) => {
            // The stream inherits the listener's non-blocking mode.
            // Switch to blocking for the handshake.
            stream.set_nonblocking(false)?;

            // Read Identify (blocking — data should be available immediately).
            match proto::decode_client(&mut stream) {
                Ok(ClientMsg::Identify { .. }) => {}
                _ => {
                    let _ = proto::send(
                        &mut stream,
                        &proto::encode_server(&ServerMsg::Error {
                            msg: "expected Identify".into(),
                        }),
                    );
                    return Ok(());
                }
            }

            // Send IdentifyAck with current pane dimensions.
            let ack = proto::encode_server(&ServerMsg::IdentifyAck {
                rows: pane.rows,
                cols: pane.cols,
            });
            if proto::send(&mut stream, &ack).is_err() {
                return Ok(());
            }

            // Send full grid snapshot.
            let snapshot = proto::encode_server(&ServerMsg::GridSnapshot {
                rows: pane.rows,
                cols: pane.cols,
                cells: pane.snapshot(),
            });
            if proto::send(&mut stream, &snapshot).is_err() {
                return Ok(());
            }

            clients.push(ClientConn::new(stream));
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            // No pending connection — spurious wakeup.
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Broadcast dirty rows + cursor to all clients. Removes clients that fail to write.
fn broadcast_grid_update(clients: &mut Vec<ClientConn>, pane: &mut Pane) -> io::Result<()> {
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

    // Send to all clients. Remove any that fail.
    let mut i = 0;
    while i < clients.len() {
        if proto::send(&mut clients[i].stream, &msg).is_err() {
            clients.remove(i);
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Broadcast a raw encoded message to all clients. Removes clients that fail.
fn broadcast_to_all(clients: &mut Vec<ClientConn>, msg: &[u8]) {
    let mut i = 0;
    while i < clients.len() {
        if proto::send(&mut clients[i].stream, msg).is_err() {
            clients.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Send a GridUpdate message to a single client (used during handshake).
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

/// Try to parse a complete frame from the buffer.
/// Returns None if the buffer doesn't contain a complete message yet.
fn try_parse_frame(buf: &mut Vec<u8>) -> io::Result<Option<ClientMsg>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let frame: Vec<u8> = buf.drain(..4 + len).collect();
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
