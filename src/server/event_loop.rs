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
    /// This client's active window index (per-client, not shared).
    active_window: usize,
}

impl ClientConn {
    fn new(stream: UnixStream) -> Self {
        let fd = stream.as_raw_fd();
        Self {
            stream,
            fd,
            buf: Vec::new(),
            active_window: 0,
        }
    }
}

/// Run the server event loop.
///
/// Phase 4: multiple windows, multiple clients, prefix-key commands.
/// Each client has its own active window. The server persists until all windows are closed.
pub fn run(listener: UnixListener, socket_path: &std::path::Path) -> io::Result<()> {
    // Wait for the first client to determine terminal size.
    let (first_stream, _) = listener.accept()?;
    let (mut grid_rows, mut grid_cols, mut windows, mut clients) =
        handshake_first_client(first_stream)?;

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
            accept_new_client(&listener, &windows, grid_rows, grid_cols, &mut clients)?;
        }

        // Window PTY output → grid → send only to clients viewing that window.
        for wi in 0..num_windows {
            let pf = &fds[1 + wi];
            if pf.revents & libc::POLLIN != 0 {
                let pane = &mut windows[wi].pane;
                match pane.process_pty_output() {
                    Ok(true) => {
                        send_grid_update_to_window_viewers(&mut clients, wi, pane)?;
                    }
                    Ok(false) => {
                        // Child exited. Remove the window.
                        windows.remove(wi);
                        if windows.is_empty() {
                            // Last window closed — shut down the server.
                            eprintln!("lrmux: last window closed, shutting down server.");
                            broadcast_to_all(
                                &mut clients,
                                &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                            );
                            ipc::cleanup(socket_path);
                            eprintln!("lrmux: server stopped.");
                            return Ok(());
                        }
                        // Fix up all clients' active_window indices.
                        for c in &mut clients {
                            if c.active_window == wi {
                                c.active_window = wi.min(windows.len() - 1);
                            } else if c.active_window > wi {
                                c.active_window -= 1;
                            }
                        }
                        // Send snapshots to all affected clients + status bar to all.
                        send_all_snapshots(&mut clients, &windows)?;
                        broadcast_status_bar(&mut clients, &windows);
                    }
                    Err(e) => {
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

        // Client input → parse frames → dispatch.
        let mut to_remove: Vec<usize> = Vec::new();
        let mut need_snapshot: Vec<usize> = Vec::new();
        let mut need_status_bar_all = false;

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
                                        let aw = clients[client_idx].active_window;
                                        if aw < windows.len() {
                                            windows[aw].pane.write_input(&data)?;
                                        }
                                    }
                                    ClientMsg::Resize { rows, cols } => {
                                        // Resize all windows. Account for status bar (1 row).
                                        grid_rows = rows.saturating_sub(1);
                                        grid_cols = cols;
                                        for w in &mut windows {
                                            w.pane.resize(grid_rows, grid_cols);
                                        }
                                        // All clients need a snapshot of their active window.
                                        for ci in 0..clients.len() {
                                            if !need_snapshot.contains(&ci) {
                                                need_snapshot.push(ci);
                                            }
                                        }
                                        need_status_bar_all = true;
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
                                        clients[client_idx].active_window = windows.len() - 1;
                                        if !need_snapshot.contains(&client_idx) {
                                            need_snapshot.push(client_idx);
                                        }
                                        // Window list changed — all clients need status bar.
                                        need_status_bar_all = true;
                                    }
                                    ClientMsg::NextWindow => {
                                        if !windows.is_empty() {
                                            let aw = clients[client_idx].active_window;
                                            clients[client_idx].active_window =
                                                (aw + 1) % windows.len();
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                        }
                                    }
                                    ClientMsg::PrevWindow => {
                                        if !windows.is_empty() {
                                            let aw = clients[client_idx].active_window;
                                            clients[client_idx].active_window =
                                                if aw == 0 { windows.len() - 1 } else { aw - 1 };
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                        }
                                    }
                                    ClientMsg::SelectWindow { index } => {
                                        if (index as usize) < windows.len() {
                                            clients[client_idx].active_window = index as usize;
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                        }
                                    }
                                    ClientMsg::KillPane => {
                                        let wi = clients[client_idx].active_window;
                                        if windows.len() > 1 {
                                            windows.remove(wi);
                                            // Fix up all clients' active_window indices.
                                            for c in &mut clients {
                                                if c.active_window == wi {
                                                    c.active_window = wi.min(windows.len() - 1);
                                                } else if c.active_window > wi {
                                                    c.active_window -= 1;
                                                }
                                            }
                                            // All clients may be affected.
                                            for ci in 0..clients.len() {
                                                if !need_snapshot.contains(&ci) {
                                                    need_snapshot.push(ci);
                                                }
                                            }
                                            need_status_bar_all = true;
                                        } else {
                                            // Last window — shut down the server.
                                            eprintln!(
                                                "lrmux: last window killed, shutting down server."
                                            );
                                            broadcast_to_all(
                                                &mut clients,
                                                &proto::encode_server(&ServerMsg::PaneExit {
                                                    code: 0,
                                                }),
                                            );
                                            ipc::cleanup(socket_path);
                                            eprintln!("lrmux: server stopped.");
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

        // Send snapshots + status bar to clients that need them.
        for &ci in &need_snapshot {
            if ci < clients.len() {
                send_snapshot_to_client(&mut clients[ci], &windows)?;
                send_status_bar_to_client(&mut clients[ci], &windows);
            }
        }

        // Broadcast status bar to all clients if window list changed.
        if need_status_bar_all {
            broadcast_status_bar(&mut clients, &windows);
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

    eprintln!("lrmux: no windows and no clients remaining, server exiting.");
    ipc::cleanup(socket_path);
    eprintln!("lrmux: server stopped.");
    Ok(())
}

/// Default name for a new window.
fn default_window_name() -> String {
    "shell".to_string()
}

/// Collect window names for the status bar.
fn window_names(windows: &[Window]) -> Vec<String> {
    windows.iter().map(|w| w.name.clone()).collect()
}

/// Broadcast status bar to all clients (per-client, using each client's active window).
fn broadcast_status_bar(clients: &mut Vec<ClientConn>, windows: &[Window]) {
    let names = window_names(windows);
    let mut i = 0;
    while i < clients.len() {
        let msg = proto::encode_server(&ServerMsg::StatusBarUpdate {
            windows: names.clone(),
            active: clients[i].active_window as u16,
        });
        if proto::send(&mut clients[i].stream, &msg).is_err() {
            clients.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Send a status bar update to a single client (using its active window).
fn send_status_bar_to_client(client: &mut ClientConn, windows: &[Window]) {
    let names = window_names(windows);
    let msg = proto::encode_server(&ServerMsg::StatusBarUpdate {
        windows: names,
        active: client.active_window as u16,
    });
    let _ = proto::send(&mut client.stream, &msg);
}

/// Send a full grid snapshot of a client's active window to that client.
fn send_snapshot_to_client(client: &mut ClientConn, windows: &[Window]) -> io::Result<()> {
    let aw = client.active_window;
    if aw >= windows.len() {
        return Ok(());
    }
    let pane = &windows[aw].pane;
    let (cursor_row, cursor_col, cursor_visible) = pane.cursor();
    let snapshot = proto::encode_server(&ServerMsg::GridSnapshot {
        rows: pane.rows,
        cols: pane.cols,
        cells: pane.snapshot(),
        cursor_row,
        cursor_col,
        cursor_visible,
    });
    proto::send(&mut client.stream, &snapshot)
}

/// Send each client a snapshot of its own active window.
fn send_all_snapshots(clients: &mut Vec<ClientConn>, windows: &[Window]) -> io::Result<()> {
    let mut i = 0;
    while i < clients.len() {
        if send_snapshot_to_client(&mut clients[i], windows).is_err() {
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
    let status = proto::encode_server(&ServerMsg::StatusBarUpdate {
        windows: vec![window.name.clone()],
        active: 0,
    });
    proto::send(&mut client, &status)?;

    Ok((
        grid_rows,
        grid_cols,
        vec![window],
        vec![ClientConn::new(client)],
    ))
}

/// Accept a new client, do the handshake, and add it to the clients list.
/// New clients default to window 0.
fn accept_new_client(
    listener: &UnixListener,
    windows: &[Window],
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

            // New client defaults to window 0.
            let active = 0usize;
            if !windows.is_empty() {
                let pane = &windows[active].pane;
                let (cursor_row, cursor_col, cursor_visible) = pane.cursor();
                let snapshot = proto::encode_server(&ServerMsg::GridSnapshot {
                    rows: pane.rows,
                    cols: pane.cols,
                    cells: pane.snapshot(),
                    cursor_row,
                    cursor_col,
                    cursor_visible,
                });
                if proto::send(&mut stream, &snapshot).is_err() {
                    return Ok(());
                }
            }

            // Send status bar.
            let status = proto::encode_server(&ServerMsg::StatusBarUpdate {
                windows: window_names(windows),
                active: active as u16,
            });
            let _ = proto::send(&mut stream, &status);

            let mut conn = ClientConn::new(stream);
            conn.active_window = active;
            clients.push(conn);
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Send dirty rows + cursor to clients viewing a specific window.
fn send_grid_update_to_window_viewers(
    clients: &mut Vec<ClientConn>,
    window_idx: usize,
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
        if clients[i].active_window == window_idx {
            if proto::send(&mut clients[i].stream, &msg).is_err() {
                clients.remove(i);
            } else {
                i += 1;
            }
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
