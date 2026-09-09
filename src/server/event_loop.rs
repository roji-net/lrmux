// Server event loop: poll over listener, all window PTY fds, and all client sockets.
//
// Supports multiple windows (each with one pane), multiple concurrent clients.
// Grid updates are broadcast only for the active window.
// A status bar (1 row) is reserved at the bottom of the client terminal.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};
use crate::server::window::Window;

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
/// Phase 4: multiple windows, multiple clients, prefix-key commands.
/// The server persists until all windows are closed.
pub fn run(listener: UnixListener, socket_path: &std::path::Path) -> io::Result<()> {
    // Wait for the first client to determine terminal size.
    let (first_stream, _) = listener.accept()?;
    let (mut grid_rows, mut grid_cols, mut windows, mut clients) =
        handshake_first_client(first_stream)?;
    let mut active_window = 0usize;

    // Set listener to non-blocking so we can poll it alongside clients.
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();

    loop {
        // Build pollfd array: listener + all window PTY fds + all client fds.
        let mut fds = Vec::with_capacity(1 + windows.len() + clients.len());
        fds.push(libc::pollfd {
            fd: listener_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        for w in &windows {
            fds.push(libc::pollfd {
                fd: w.pty_fd(),
                events: libc::POLLIN,
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

        let num_windows = windows.len();
        let polled_clients = clients.len();

        // Listener readable → accept new client.
        if fds[0].revents & libc::POLLIN != 0 {
            accept_new_client(
                &listener,
                &windows,
                active_window,
                grid_rows,
                grid_cols,
                &mut clients,
            )?;
        }

        // Window PTY output → grid → broadcast (only active window).
        for wi in 0..num_windows {
            let pf = &fds[1 + wi];
            if pf.revents & libc::POLLIN != 0 {
                let pane = &mut windows[wi].pane;
                match pane.process_pty_output() {
                    Ok(true) => {
                        if wi == active_window {
                            broadcast_grid_update(&mut clients, pane)?;
                        }
                    }
                    Ok(false) => {
                        // Child exited. Remove the window.
                        windows.remove(wi);
                        if windows.is_empty() {
                            // Last window closed — shut down the server.
                            broadcast_to_all(
                                &mut clients,
                                &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                            );
                            ipc::cleanup(socket_path);
                            return Ok(());
                        }
                        if wi <= active_window {
                            active_window = active_window.saturating_sub(1);
                        }
                        // Switch to the new active window: send snapshot + status bar.
                        send_window_snapshot(&mut clients, &windows, active_window)?;
                        broadcast_status_bar(&mut clients, &windows, active_window);
                    }
                    Err(e) => {
                        if wi == active_window {
                            broadcast_to_all(
                                &mut clients,
                                &proto::encode_server(&ServerMsg::Error {
                                    msg: format!("pty read error: {e}"),
                                }),
                            );
                        }
                    }
                }
            }
        }

        // Client input → parse frames → dispatch.
        let mut to_remove: Vec<usize> = Vec::new();
        let mut window_changed = false;
        let mut need_resize = false;

        for client_idx in 0..polled_clients {
            let pf = &fds[1 + num_windows + client_idx];
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
                    loop {
                        match try_parse_frame(&mut clients[client_idx].buf) {
                            Ok(Some(msg)) => {
                                match msg {
                                    ClientMsg::PaneInput { data } => {
                                        windows[active_window].pane.write_input(&data)?;
                                    }
                                    ClientMsg::Resize { rows, cols } => {
                                        // Resize all windows. Account for status bar (1 row).
                                        grid_rows = rows.saturating_sub(1);
                                        grid_cols = cols;
                                        for w in &mut windows {
                                            w.pane.resize(grid_rows, grid_cols);
                                        }
                                        need_resize = true;
                                    }
                                    ClientMsg::Detach => {
                                        to_remove.push(client_idx);
                                        break;
                                    }
                                    ClientMsg::NewWindow => {
                                        windows.push(Window::new(
                                            grid_rows,
                                            grid_cols,
                                            default_window_name(),
                                        ));
                                        active_window = windows.len() - 1;
                                        window_changed = true;
                                    }
                                    ClientMsg::NextWindow => {
                                        if !windows.is_empty() {
                                            active_window = (active_window + 1) % windows.len();
                                            window_changed = true;
                                        }
                                    }
                                    ClientMsg::PrevWindow => {
                                        if !windows.is_empty() {
                                            active_window = if active_window == 0 {
                                                windows.len() - 1
                                            } else {
                                                active_window - 1
                                            };
                                            window_changed = true;
                                        }
                                    }
                                    ClientMsg::SelectWindow { index } => {
                                        if (index as usize) < windows.len() {
                                            active_window = index as usize;
                                            window_changed = true;
                                        }
                                    }
                                    ClientMsg::KillPane => {
                                        if windows.len() > 1 {
                                            windows.remove(active_window);
                                            if active_window >= windows.len() {
                                                active_window = windows.len() - 1;
                                            }
                                            window_changed = true;
                                        } else {
                                            // Last window — shut down the server.
                                            broadcast_to_all(
                                                &mut clients,
                                                &proto::encode_server(&ServerMsg::PaneExit {
                                                    code: 0,
                                                }),
                                            );
                                            ipc::cleanup(socket_path);
                                            return Ok(());
                                        }
                                    }
                                    ClientMsg::Identify { .. } => {}
                                }
                            }
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
            if pf.revents & (libc::POLLHUP | libc::POLLERR) != 0 && !to_remove.contains(&client_idx)
            {
                to_remove.push(client_idx);
            }
        }

        // Handle resize: mark all dirty, broadcast active window snapshot.
        if need_resize {
            for w in &mut windows {
                w.pane.grid.mark_all_dirty();
            }
            send_window_snapshot(&mut clients, &windows, active_window)?;
            broadcast_status_bar(&mut clients, &windows, active_window);
        }

        // Handle window switch: send snapshot of new active window + status bar.
        if window_changed {
            send_window_snapshot(&mut clients, &windows, active_window)?;
            broadcast_status_bar(&mut clients, &windows, active_window);
        }

        // Remove disconnected clients (in reverse order to preserve indices).
        for &idx in to_remove.iter().rev() {
            if idx < clients.len() {
                clients.remove(idx);
            }
        }

        // If no windows and no clients, exit.
        if windows.is_empty() && clients.is_empty() {
            break;
        }
    }

    ipc::cleanup(socket_path);
    Ok(())
}

/// Default name for a new window.
fn default_window_name() -> String {
    "shell".to_string()
}

/// Build the status bar text from the window list.
fn status_bar_text(windows: &[&Window], active: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (i, w) in windows.iter().enumerate() {
        if i == active {
            parts.push(format!("{}:{}*", i, w.name));
        } else {
            parts.push(format!("{}:{}", i, w.name));
        }
    }
    format!("lrmux | {}", parts.join("  "))
}

/// Broadcast status bar to all clients.
fn broadcast_status_bar(clients: &mut Vec<ClientConn>, windows: &[Window], active: usize) {
    let refs: Vec<&Window> = windows.iter().collect();
    let text = status_bar_text(&refs, active);
    let msg = proto::encode_server(&ServerMsg::StatusBarUpdate { text });
    broadcast_to_all(clients, &msg);
}

/// Send a full grid snapshot of the active window to all clients.
fn send_window_snapshot(
    clients: &mut Vec<ClientConn>,
    windows: &[Window],
    active: usize,
) -> io::Result<()> {
    if windows.is_empty() {
        return Ok(());
    }
    let pane = &windows[active].pane;
    let snapshot = proto::encode_server(&ServerMsg::GridSnapshot {
        rows: pane.rows,
        cols: pane.cols,
        cells: pane.snapshot(),
    });
    let mut i = 0;
    while i < clients.len() {
        if proto::send(&mut clients[i].stream, &snapshot).is_err() {
            clients.remove(i);
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Handshake with the first client: read Identify, create first window, send ack + snapshot.
fn handshake_first_client(
    stream: UnixStream,
) -> io::Result<(u16, u16, Vec<Window>, Vec<ClientConn>)> {
    let mut client = stream;

    let (client_rows, client_cols) = match proto::decode_client(&mut client) {
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

    // Reserve 1 row for the status bar.
    let grid_rows = client_rows.saturating_sub(1);
    let grid_cols = client_cols;

    // Create the first window.
    let mut window = Window::new(grid_rows, grid_cols, default_window_name());

    // Send IdentifyAck with grid dimensions (not client dimensions).
    let ack = proto::encode_server(&ServerMsg::IdentifyAck {
        rows: grid_rows,
        cols: grid_cols,
    });
    proto::send(&mut client, &ack)?;

    // Send full grid snapshot.
    window.pane.grid.mark_all_dirty();
    send_grid_update(&mut client, &mut window.pane)?;

    // Send status bar.
    let status_text = status_bar_text(&[&window], 0);
    let status = proto::encode_server(&ServerMsg::StatusBarUpdate { text: status_text });
    proto::send(&mut client, &status)?;

    Ok((
        grid_rows,
        grid_cols,
        vec![window],
        vec![ClientConn::new(client)],
    ))
}

/// Accept a new client, do the handshake, and add it to the clients list.
fn accept_new_client(
    listener: &UnixListener,
    windows: &[Window],
    active_window: usize,
    grid_rows: u16,
    grid_cols: u16,
    clients: &mut Vec<ClientConn>,
) -> io::Result<()> {
    match listener.accept() {
        Ok((mut stream, _)) => {
            stream.set_nonblocking(false)?;

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

            // Send IdentifyAck with grid dimensions.
            let ack = proto::encode_server(&ServerMsg::IdentifyAck {
                rows: grid_rows,
                cols: grid_cols,
            });
            if proto::send(&mut stream, &ack).is_err() {
                return Ok(());
            }

            // Send full grid snapshot of the active window.
            if !windows.is_empty() {
                let pane = &windows[active_window].pane;
                let snapshot = proto::encode_server(&ServerMsg::GridSnapshot {
                    rows: pane.rows,
                    cols: pane.cols,
                    cells: pane.snapshot(),
                });
                if proto::send(&mut stream, &snapshot).is_err() {
                    return Ok(());
                }
            }

            // Send status bar.
            let refs: Vec<&Window> = windows.iter().collect();
            let status = proto::encode_server(&ServerMsg::StatusBarUpdate {
                text: status_bar_text(&refs, active_window),
            });
            let _ = proto::send(&mut stream, &status);

            clients.push(ClientConn::new(stream));
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Broadcast dirty rows + cursor to all clients. Removes clients that fail to write.
fn broadcast_grid_update(
    clients: &mut Vec<ClientConn>,
    pane: &mut crate::server::pane::Pane,
) -> io::Result<()> {
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
fn send_grid_update<W: Write>(
    writer: &mut W,
    pane: &mut crate::server::pane::Pane,
) -> io::Result<()> {
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
        0x05 => ClientMsg::NewWindow,
        0x06 => ClientMsg::NextWindow,
        0x07 => ClientMsg::PrevWindow,
        0x08 => {
            if data.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SelectWindow needs 1 byte",
                ));
            }
            ClientMsg::SelectWindow { index: data[0] }
        }
        0x09 => ClientMsg::KillPane,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown client msg type: {msg_type}"),
            ));
        }
    };
    Ok(Some(msg))
}
