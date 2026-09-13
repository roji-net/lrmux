// Server event loop: poll over listener, all window PTY fds, and all client sockets.
//
// Supports multiple sessions, multiple windows per session, multiple concurrent clients.
// Each client has its own active session and active window within that session.
// Grid updates are sent only to clients viewing the relevant window.
// A status bar (1 row) is reserved at the bottom of the client terminal.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};
use crate::pty;
use crate::server::session::Session;
use crate::server::window::Window;

/// A connected client.
struct ClientConn {
    stream: UnixStream,
    fd: i32,
    buf: Vec<u8>,
    /// Which session this client is attached to.
    session_idx: usize,
    /// This client's active window index within its session (per-client, not shared).
    active_window: usize,
}

impl ClientConn {
    fn new(stream: UnixStream) -> Self {
        let fd = stream.as_raw_fd();
        Self {
            stream,
            fd,
            buf: Vec::new(),
            session_idx: 0,
            active_window: 0,
        }
    }
}

/// Run the server event loop.
///
/// Phase 4: multiple sessions, multiple windows, multiple clients, prefix-key commands.
/// Each client has its own active session and active window. The server persists until
/// all sessions are closed.
pub fn run(listener: UnixListener, socket_path: &std::path::Path) -> io::Result<()> {
    // Wait for the first client to determine terminal size.
    // Retry on bad connections (e.g. ListSessions queries from the selector,
    // or connections that send unexpected data).
    let (mut grid_rows, mut grid_cols, mut sessions, mut clients) = loop {
        let (stream, _) = listener.accept()?;
        match handshake_first_client(stream) {
            Ok(result) => break result,
            Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                // KillServer received during initial handshake — shut down.
                crate::log::info("KillServer during initial handshake, shutting down");
                ipc::cleanup(socket_path);
                return Ok(());
            }
            Err(e) => {
                eprintln!(
                    "lrmux server: initial handshake failed ({e}), waiting for next client..."
                );
                continue;
            }
        }
    };

    // Set listener to non-blocking so we can poll it alongside clients.
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();

    loop {
        // Build pollfd array: listener + all window PTY fds (across all sessions) + all client fds.
        // We need a mapping from pollfd index to (session_idx, window_idx).
        let mut pty_map: Vec<(usize, usize)> = Vec::new();
        let mut fds = Vec::with_capacity(1 + 64 + clients.len());
        fds.push(libc::pollfd {
            fd: listener_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        for (si, session) in sessions.iter().enumerate() {
            for (wi, w) in session.windows.iter().enumerate() {
                // Skip exited panes — their child is dead, no more PTY output.
                if w.pane.is_exited() {
                    continue;
                }
                pty_map.push((si, wi));
                fds.push(libc::pollfd {
                    fd: w.pty_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }
        let num_pty_fds = pty_map.len();
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

        let polled_clients = clients.len();

        // Listener readable → accept new client.
        if fds[0].revents & libc::POLLIN != 0 {
            match accept_new_client(&listener, &sessions, grid_rows, grid_cols, &mut clients) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                    // KillServer received — shut down gracefully.
                    crate::log::info("KillServer received, shutting down");
                    kill_all_children(&sessions);
                    broadcast_to_all(
                        &mut clients,
                        &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                    );
                    ipc::cleanup(socket_path);
                    return Ok(());
                }
                Err(_) => {}
            }
        }

        // Window PTY output → grid → send only to clients viewing that window.
        // First pass: read all PTYs, collect grid updates and child exits.
        // We must NOT modify sessions/windows during this pass because
        // pty_map indices would become stale for subsequent PTYs.
        let mut pty_exits: Vec<(usize, usize, i32)> = Vec::new();
        for pty_i in 0..num_pty_fds {
            let pf = &fds[1 + pty_i];
            if pf.revents & libc::POLLIN != 0 {
                let (si, wi) = pty_map[pty_i];
                // Bounds-check against current sessions/windows (safety: a
                // previous exit in this same poll iteration may have removed
                // a session/window, making this index stale).
                if si >= sessions.len() || wi >= sessions[si].windows.len() {
                    continue;
                }
                let session = &mut sessions[si];
                let pane = &mut session.windows[wi].pane;
                match pane.process_pty_output() {
                    Ok(true) => {
                        let _ = send_grid_update_to_window_viewers(&mut clients, si, wi, pane);
                    }
                    Ok(false) => {
                        // Child exited — reap and defer the removal.
                        let exit_code = pane.reap_child().unwrap_or(0);
                        pty_exits.push((si, wi, exit_code));
                    }
                    Err(e) => {
                        crate::log::error(&format!("pty read error: {e}"));
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

        // Second pass: process child exits.
        // Sort in reverse order (highest si first, then highest wi) so removals
        // don't invalidate lower indices.
        pty_exits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        for (si, wi, exit_code) in pty_exits {
            // Bounds-check again (a previous removal in this loop may have shifted indices).
            if si >= sessions.len() {
                continue;
            }
            let session = &mut sessions[si];
            if wi >= session.windows.len() {
                continue;
            }
            let session_name = session.name.clone();
            crate::log::info(&format!(
                "child exited in session '{session_name}' window {wi}: code {exit_code}"
            ));
            // Treat 0 and 130 (128+SIGINT, common when exiting shells
            // with Ctrl-D after a Ctrl-C) and -2 (direct SIGINT signal)
            // as success — auto-close the window.
            if exit_code == 0 || exit_code == 130 || exit_code == -2 {
                // Exit code 0: auto-close the window.
                session.windows.remove(wi);
                if session.windows.is_empty() {
                    // Last window in this session closed — remove the session.
                    crate::log::info(&format!(
                        "last window in session '{session_name}' closed, removing session"
                    ));
                    sessions.remove(si);
                    if sessions.is_empty() {
                        // Last session closed — shut down the server.
                        crate::log::info("last session closed, shutting down server");
                        broadcast_to_all(
                            &mut clients,
                            &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                        );
                        ipc::cleanup(socket_path);
                        return Ok(());
                    }
                    // Fix up all clients' session indices.
                    for c in &mut clients {
                        if c.session_idx == si {
                            // Client was in the removed session — move to session 0.
                            c.session_idx = 0;
                            c.active_window = 0;
                        } else if c.session_idx > si {
                            c.session_idx -= 1;
                        }
                    }
                    // All clients need a snapshot + status bar (their view changed).
                    let _ = send_all_snapshots(&mut clients, &sessions);
                    broadcast_status_bar(&mut clients, &sessions);
                } else {
                    // Fix up all clients' active_window indices in this session.
                    for c in &mut clients {
                        if c.session_idx == si {
                            if c.active_window == wi {
                                c.active_window = wi.min(session.windows.len() - 1);
                            } else if c.active_window > wi {
                                c.active_window -= 1;
                            }
                        }
                    }
                    // Send snapshots to affected clients + status bar to all.
                    let _ = send_all_snapshots(&mut clients, &sessions);
                    broadcast_status_bar(&mut clients, &sessions);
                }
            } else {
                // Exit code ≠ 0: keep the pane open with an exit message.
                // The user can read the output and close manually with Prefix x.
                let pane = &mut sessions[si].windows[wi].pane;
                pane.write_exit_message(exit_code);
                let _ = send_grid_update_to_window_viewers(&mut clients, si, wi, pane);
            }
        }

        // Client input → parse frames → dispatch.
        let mut to_remove: Vec<usize> = Vec::new();
        let mut need_snapshot: Vec<usize> = Vec::new();
        let mut need_status_bar_all = false;

        for client_idx in 0..polled_clients {
            let pf = &fds[1 + num_pty_fds + client_idx];
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
                                        let si = clients[client_idx].session_idx;
                                        let aw = clients[client_idx].active_window;
                                        if si < sessions.len() && aw < sessions[si].windows.len() {
                                            let _ =
                                                sessions[si].windows[aw].pane.write_input(&data);
                                        }
                                    }
                                    ClientMsg::Resize { rows, cols } => {
                                        // Explicit canonical resize (Prefix F).
                                        // Only resize the current session's panes, not all sessions.
                                        let si = clients[client_idx].session_idx;
                                        let new_grid_rows = rows.saturating_sub(1);
                                        let new_grid_cols = cols;
                                        if si < sessions.len() {
                                            for w in &mut sessions[si].windows {
                                                w.pane.resize(new_grid_rows, new_grid_cols);
                                            }
                                        }
                                        // Update global grid size if the first session was resized
                                        // (used for new windows/sessions created after this point).
                                        if si == 0 {
                                            grid_rows = new_grid_rows;
                                            grid_cols = new_grid_cols;
                                        }
                                        // Send snapshots to all clients viewing this session.
                                        for (ci, client) in clients.iter().enumerate() {
                                            if client.session_idx == si
                                                && !need_snapshot.contains(&ci)
                                            {
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
                                        let si = clients[client_idx].session_idx;
                                        if si < sessions.len() {
                                            // Get the CWD of the active window's child
                                            // process so the new window opens in the
                                            // same directory.
                                            let aw = clients[client_idx].active_window;
                                            let cwd = if aw < sessions[si].windows.len() {
                                                let pane = &sessions[si].windows[aw].pane;
                                                if !pane.exited {
                                                    pty::child_cwd_full(pane.pty.child_pid)
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            };
                                            let win = match cwd {
                                                Some(ref dir) => Window::new_in_cwd(
                                                    grid_rows,
                                                    grid_cols,
                                                    default_window_name(),
                                                    dir,
                                                ),
                                                None => Window::new(
                                                    grid_rows,
                                                    grid_cols,
                                                    default_window_name(),
                                                ),
                                            };
                                            sessions[si].windows.push(win);
                                            let new_wi = sessions[si].windows.len() - 1;
                                            let sname = sessions[si].name.clone();
                                            crate::log::info(&format!(
                                                "new window {new_wi} in session '{sname}'"
                                            ));
                                            clients[client_idx].active_window = new_wi;
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                            need_status_bar_all = true;
                                        }
                                    }
                                    ClientMsg::NextWindow => {
                                        let si = clients[client_idx].session_idx;
                                        if si < sessions.len() && !sessions[si].windows.is_empty() {
                                            let aw = clients[client_idx].active_window;
                                            clients[client_idx].active_window =
                                                (aw + 1) % sessions[si].windows.len();
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                        }
                                    }
                                    ClientMsg::PrevWindow => {
                                        let si = clients[client_idx].session_idx;
                                        if si < sessions.len() && !sessions[si].windows.is_empty() {
                                            let aw = clients[client_idx].active_window;
                                            let len = sessions[si].windows.len();
                                            clients[client_idx].active_window =
                                                if aw == 0 { len - 1 } else { aw - 1 };
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                        }
                                    }
                                    ClientMsg::SelectWindow { index } => {
                                        let si = clients[client_idx].session_idx;
                                        if si < sessions.len()
                                            && (index as usize) < sessions[si].windows.len()
                                        {
                                            clients[client_idx].active_window = index as usize;
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                        }
                                    }
                                    ClientMsg::KillPane => {
                                        let si = clients[client_idx].session_idx;
                                        let wi = clients[client_idx].active_window;
                                        if si >= sessions.len() {
                                            continue;
                                        }
                                        let session = &mut sessions[si];
                                        if wi >= session.windows.len() {
                                            continue;
                                        }
                                        let session_name = session.name.clone();
                                        crate::log::info(&format!(
                                            "kill pane: session '{session_name}' window {wi}"
                                        ));
                                        if session.windows.len() > 1 {
                                            session.windows.remove(wi);
                                            // Fix up all clients' active_window in this session.
                                            for c in &mut clients {
                                                if c.session_idx == si {
                                                    if c.active_window == wi {
                                                        c.active_window =
                                                            wi.min(session.windows.len() - 1);
                                                    } else if c.active_window > wi {
                                                        c.active_window -= 1;
                                                    }
                                                }
                                            }
                                            for ci in 0..clients.len() {
                                                if !need_snapshot.contains(&ci) {
                                                    need_snapshot.push(ci);
                                                }
                                            }
                                            need_status_bar_all = true;
                                        } else {
                                            // Last window in this session — remove the session.
                                            crate::log::info(&format!(
                                                "last window in session '{session_name}' killed, removing session"
                                            ));
                                            sessions.remove(si);
                                            if sessions.is_empty() {
                                                crate::log::info(
                                                    "last session closed, shutting down server",
                                                );
                                                broadcast_to_all(
                                                    &mut clients,
                                                    &proto::encode_server(&ServerMsg::PaneExit {
                                                        code: 0,
                                                    }),
                                                );
                                                ipc::cleanup(socket_path);
                                                return Ok(());
                                            }
                                            // Fix up all clients' session indices.
                                            for c in &mut clients {
                                                if c.session_idx == si {
                                                    c.session_idx = 0;
                                                    c.active_window = 0;
                                                } else if c.session_idx > si {
                                                    c.session_idx -= 1;
                                                }
                                            }
                                            for ci in 0..clients.len() {
                                                if !need_snapshot.contains(&ci) {
                                                    need_snapshot.push(ci);
                                                }
                                            }
                                            need_status_bar_all = true;
                                        }
                                    }
                                    ClientMsg::NewSession { name } => {
                                        // Use the CWD of the active window's child process
                                        // as the default session name and working directory.
                                        let si = clients[client_idx].session_idx;
                                        let wi = clients[client_idx].active_window;
                                        let cwd_full = if si < sessions.len()
                                            && wi < sessions[si].windows.len()
                                        {
                                            let pane = &sessions[si].windows[wi].pane;
                                            if !pane.exited {
                                                pty::child_cwd_full(pane.pty.child_pid)
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        };
                                        let session_name =
                                            name.unwrap_or_else(|| match &cwd_full {
                                                Some(cwd) => {
                                                    let base = std::path::Path::new(cwd)
                                                        .file_name()
                                                        .map(|n| n.to_string_lossy().into_owned())
                                                        .unwrap_or_else(|| "session".to_string());
                                                    ensure_unique_session_name(&base, &sessions)
                                                }
                                                None => default_session_name(&sessions),
                                            });
                                        let session = match &cwd_full {
                                            Some(cwd) => Session::new_in_cwd(
                                                session_name,
                                                grid_rows,
                                                grid_cols,
                                                cwd,
                                            ),
                                            None => {
                                                Session::new(session_name, grid_rows, grid_cols)
                                            }
                                        };
                                        sessions.push(session);
                                        let new_si = sessions.len() - 1;
                                        let new_name = sessions[new_si].name.clone();
                                        crate::log::info(&format!(
                                            "new session '{new_name}' created (index {new_si})"
                                        ));
                                        clients[client_idx].session_idx = new_si;
                                        clients[client_idx].active_window = 0;
                                        if !need_snapshot.contains(&client_idx) {
                                            need_snapshot.push(client_idx);
                                        }
                                        need_status_bar_all = true;
                                    }
                                    ClientMsg::NextSession => {
                                        if sessions.len() > 1 {
                                            let si = clients[client_idx].session_idx;
                                            clients[client_idx].session_idx =
                                                (si + 1) % sessions.len();
                                            clients[client_idx].active_window = 0;
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                            need_status_bar_all = true;
                                        }
                                    }
                                    ClientMsg::PrevSession => {
                                        if sessions.len() > 1 {
                                            let si = clients[client_idx].session_idx;
                                            clients[client_idx].session_idx =
                                                if si == 0 { sessions.len() - 1 } else { si - 1 };
                                            clients[client_idx].active_window = 0;
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                            need_status_bar_all = true;
                                        }
                                    }
                                    ClientMsg::SelectSession { name } => {
                                        if let Some(idx) =
                                            sessions.iter().position(|s| s.name == name)
                                        {
                                            clients[client_idx].session_idx = idx;
                                            clients[client_idx].active_window = 0;
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                            need_status_bar_all = true;
                                        }
                                    }
                                    ClientMsg::KillSession => {
                                        let si = clients[client_idx].session_idx;
                                        if si >= sessions.len() {
                                            continue;
                                        }
                                        let name = sessions[si].name.clone();
                                        crate::log::info(&format!(
                                            "session '{name}' killed by client, removing"
                                        ));
                                        sessions.remove(si);
                                        if sessions.is_empty() {
                                            crate::log::info(
                                                "last session closed, shutting down server",
                                            );
                                            kill_all_children(&sessions);
                                            broadcast_to_all(
                                                &mut clients,
                                                &proto::encode_server(&ServerMsg::PaneExit {
                                                    code: 0,
                                                }),
                                            );
                                            ipc::cleanup(socket_path);
                                            return Ok(());
                                        }
                                        // Fix up all clients' session indices.
                                        for c in &mut clients {
                                            if c.session_idx == si {
                                                c.session_idx = 0;
                                                c.active_window = 0;
                                            } else if c.session_idx > si {
                                                c.session_idx -= 1;
                                            }
                                        }
                                        for ci in 0..clients.len() {
                                            if !need_snapshot.contains(&ci) {
                                                need_snapshot.push(ci);
                                            }
                                        }
                                        need_status_bar_all = true;
                                    }
                                    ClientMsg::RenameSession { name } => {
                                        let si = clients[client_idx].session_idx;
                                        if si < sessions.len() {
                                            sessions[si].name = name;
                                            need_status_bar_all = true;
                                        }
                                    }
                                    ClientMsg::Identify { .. } => {}
                                    ClientMsg::ListSessions => {
                                        let names: Vec<String> =
                                            sessions.iter().map(|s| s.name.clone()).collect();
                                        let msg = proto::encode_server(&ServerMsg::SessionList {
                                            sessions: names,
                                        });
                                        let _ = proto::send(&mut clients[client_idx].stream, &msg);
                                    }
                                    ClientMsg::KillServer => {
                                        eprintln!("lrmux: KillServer received, shutting down.");
                                        broadcast_to_all(
                                            &mut clients,
                                            &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                                        );
                                        ipc::cleanup(socket_path);
                                        eprintln!("lrmux: server stopped.");
                                        return Ok(());
                                    }
                                    ClientMsg::NewWindowIn { session, command } => {
                                        // Find the target session by name, or use the first.
                                        let si = match session {
                                            Some(ref name) => {
                                                sessions.iter().position(|s| &s.name == name)
                                            }
                                            None => {
                                                if sessions.is_empty() {
                                                    None
                                                } else {
                                                    Some(0)
                                                }
                                            }
                                        };
                                        if let Some(si) = si {
                                            let win = match command {
                                                Some(ref cmd) => Window::new_with_command(
                                                    grid_rows,
                                                    grid_cols,
                                                    default_window_name(),
                                                    cmd,
                                                ),
                                                None => Window::new(
                                                    grid_rows,
                                                    grid_cols,
                                                    default_window_name(),
                                                ),
                                            };
                                            sessions[si].windows.push(win);
                                            need_status_bar_all = true;
                                        }
                                    }
                                    ClientMsg::CaptureWindow { session, window } => {
                                        // Find the target session by name, or use the first.
                                        let si = match session {
                                            Some(ref name) => {
                                                sessions.iter().position(|s| &s.name == name)
                                            }
                                            None => {
                                                if sessions.is_empty() {
                                                    None
                                                } else {
                                                    Some(0)
                                                }
                                            }
                                        };
                                        let content = if let Some(si) = si {
                                            let wi = match window {
                                                Some(w) => Some(w as usize),
                                                None => Some(clients[client_idx].active_window),
                                            };
                                            if let Some(wi) = wi
                                                && wi < sessions[si].windows.len()
                                            {
                                                Some(render_grid_text(
                                                    &sessions[si].windows[wi].pane,
                                                ))
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        };
                                        let msg = proto::encode_server(&ServerMsg::WindowCapture {
                                            content: content.unwrap_or_default(),
                                        });
                                        let _ = proto::send(&mut clients[client_idx].stream, &msg);
                                    }
                                    ClientMsg::SendKeys {
                                        session,
                                        window,
                                        keys,
                                    } => {
                                        // Find the target session by name, or use the first.
                                        let si = match session {
                                            Some(ref name) => {
                                                sessions.iter().position(|s| &s.name == name)
                                            }
                                            None => {
                                                if sessions.is_empty() {
                                                    None
                                                } else {
                                                    Some(0)
                                                }
                                            }
                                        };
                                        if let Some(si) = si {
                                            let wi = match window {
                                                Some(w) => Some(w as usize),
                                                None => Some(clients[client_idx].active_window),
                                            };
                                            if let Some(wi) = wi
                                                && wi < sessions[si].windows.len()
                                            {
                                                let _ = sessions[si].windows[wi]
                                                    .pane
                                                    .write_input(&keys);
                                            }
                                        }
                                    }
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
        // Handle send failures gracefully — remove the client instead of killing the server.
        let mut snapshot_failures: Vec<usize> = Vec::new();
        for &ci in &need_snapshot {
            if ci < clients.len() {
                if send_snapshot_to_client(&mut clients[ci], &sessions).is_err() {
                    snapshot_failures.push(ci);
                } else {
                    send_status_bar_to_client(&mut clients[ci], &sessions);
                }
            }
        }
        // Mark failed clients for removal.
        for ci in snapshot_failures {
            if !to_remove.contains(&ci) {
                to_remove.push(ci);
            }
        }

        // Broadcast status bar to all clients if session/window list changed.
        if need_status_bar_all {
            broadcast_status_bar(&mut clients, &sessions);
        }

        // Remove disconnected clients (in reverse order to preserve indices).
        for &idx in to_remove.iter().rev() {
            if idx < clients.len() {
                crate::log::info(&format!(
                    "client {idx} disconnected, {} remaining",
                    clients.len() - 1
                ));
                clients.remove(idx);
            }
        }

        // If no sessions and no clients, exit.
        if sessions.is_empty() && clients.is_empty() {
            break;
        }
    }

    crate::log::info("no sessions and no clients remaining, server exiting");
    // Send SIGHUP to all remaining child processes before cleanup.
    kill_all_children(&sessions);
    ipc::cleanup(socket_path);
    Ok(())
}

/// Send SIGHUP to all living child processes across all sessions.
/// This ensures children (shells, AI CLIs, etc.) are notified when the
/// server is shutting down, rather than being orphaned silently.
fn kill_all_children(sessions: &[Session]) {
    for session in sessions {
        for w in &session.windows {
            if !w.pane.exited {
                crate::log::info(&format!(
                    "SIGHUP child pid {} in session '{}' window",
                    w.pane.pty.child_pid, session.name
                ));
                crate::pty::kill_child(w.pane.pty.child_pid, libc::SIGHUP);
            }
        }
    }
}

/// Default name for a new window.
fn default_window_name() -> String {
    "shell".to_string()
}

/// Generate a default session name based on the current directory.
/// Uses the directory basename; if that's taken, appends -2, -3, etc.
fn default_session_name(sessions: &[Session]) -> String {
    let base = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "session".to_string());
    ensure_unique_session_name(&base, sessions)
}

/// Render a pane's grid as plain text (for capture-window).
/// Each row is trimmed of trailing whitespace and joined with newlines.
fn render_grid_text(pane: &crate::server::pane::Pane) -> String {
    let rows = pane.rows as usize;
    let cols = pane.cols as usize;
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut line = String::with_capacity(cols);
        if let Some(r) = pane.grid.row(row) {
            for c in r.iter().take(cols) {
                let ch = if c.ch == '\0' { ' ' } else { c.ch };
                line.push(ch);
            }
        }
        // Trim trailing whitespace.
        lines.push(line.trim_end().to_string());
    }
    // Trim trailing empty lines.
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.join("\n")
}

/// Ensure a session name is unique by appending -2, -3, etc. if needed.
fn ensure_unique_session_name(base: &str, sessions: &[Session]) -> String {
    if sessions.iter().all(|s| s.name != base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if sessions.iter().all(|s| s.name != candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Collect window names for the status bar from a client's session.
fn window_names(session: &Session) -> Vec<String> {
    session.windows.iter().map(|w| w.name.clone()).collect()
}

/// Broadcast status bar to all clients (per-client, using each client's session + active window).
fn broadcast_status_bar(clients: &mut Vec<ClientConn>, sessions: &[Session]) {
    let mut i = 0;
    while i < clients.len() {
        let si = clients[i].session_idx;
        if si >= sessions.len() {
            i += 1;
            continue;
        }
        let msg = proto::encode_server(&ServerMsg::StatusBarUpdate {
            session: sessions[si].name.clone(),
            windows: window_names(&sessions[si]),
            active: clients[i].active_window as u16,
            session_count: sessions.len() as u16,
        });
        if proto::send(&mut clients[i].stream, &msg).is_err() {
            clients.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Send a status bar update to a single client (using its session + active window).
fn send_status_bar_to_client(client: &mut ClientConn, sessions: &[Session]) {
    let si = client.session_idx;
    if si >= sessions.len() {
        return;
    }
    let msg = proto::encode_server(&ServerMsg::StatusBarUpdate {
        session: sessions[si].name.clone(),
        windows: window_names(&sessions[si]),
        active: client.active_window as u16,
        session_count: sessions.len() as u16,
    });
    let _ = proto::send(&mut client.stream, &msg);
}

/// Send a full grid snapshot of a client's active window to that client.
fn send_snapshot_to_client(client: &mut ClientConn, sessions: &[Session]) -> io::Result<()> {
    let si = client.session_idx;
    let aw = client.active_window;
    if si >= sessions.len() || aw >= sessions[si].windows.len() {
        return Ok(());
    }
    let pane = &sessions[si].windows[aw].pane;
    let (cursor_row, cursor_col, cursor_visible) = pane.cursor();

    // Send the grid snapshot first (client creates a fresh grid).
    let snapshot = proto::encode_server(&ServerMsg::GridSnapshot {
        rows: pane.rows,
        cols: pane.cols,
        cells: pane.snapshot(),
        cursor_row,
        cursor_col,
        cursor_visible,
    });
    if proto::send(&mut client.stream, &snapshot).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "client gone",
        ));
    }

    // Then send scrollback so the client can populate the fresh grid's history.
    let sb_rows = pane.scrollback_rows();
    if !sb_rows.is_empty() {
        let sb_msg = proto::encode_server(&ServerMsg::ScrollbackUpdate { rows: sb_rows });
        let _ = proto::send(&mut client.stream, &sb_msg);
    }
    Ok(())
}

/// Send each client a snapshot of its own active window.
fn send_all_snapshots(clients: &mut Vec<ClientConn>, sessions: &[Session]) -> io::Result<()> {
    let mut i = 0;
    while i < clients.len() {
        if send_snapshot_to_client(&mut clients[i], sessions).is_err() {
            clients.remove(i);
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Handshake with the first client: read Identify, create first session, send ack + snapshot.
/// Returns Err if the connection is not an Identify (e.g. ListSessions query or bad data).
/// The caller should retry by accepting the next connection.
fn handshake_first_client(
    stream: UnixStream,
) -> io::Result<(u16, u16, Vec<Session>, Vec<ClientConn>)> {
    let mut client = stream;

    let (client_rows, client_cols) = match proto::decode_client(&mut client) {
        Ok(ClientMsg::Identify { rows, cols }) => (rows, cols),
        Ok(ClientMsg::ListSessions) => {
            // Respond with empty session list (no sessions yet) and signal retry.
            let msg = proto::encode_server(&ServerMsg::SessionList { sessions: vec![] });
            let _ = proto::send(&mut client, &msg);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ListSessions query during initial handshake",
            ));
        }
        Ok(ClientMsg::KillServer) => {
            eprintln!("lrmux: KillServer received during initial handshake, shutting down.");
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "KillServer",
            ));
        }
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

    // Create the first session, named after the current directory.
    let session_name = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "session".to_string());
    let mut session = Session::new(session_name, grid_rows, grid_cols);
    let window = &mut session.windows[0];

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
        session: session.name.clone(),
        windows: vec![window.name.clone()],
        active: 0,
        session_count: 1,
    });
    proto::send(&mut client, &status)?;

    let mut conn = ClientConn::new(client);
    conn.session_idx = 0;
    conn.active_window = 0;

    Ok((grid_rows, grid_cols, vec![session], vec![conn]))
}

/// Accept a new client, do the handshake, and add it to the clients list.
/// New clients default to session 0, window 0.
fn accept_new_client(
    listener: &UnixListener,
    sessions: &[Session],
    grid_rows: u16,
    grid_cols: u16,
    clients: &mut Vec<ClientConn>,
) -> io::Result<()> {
    match listener.accept() {
        Ok((mut stream, _)) => {
            stream.set_nonblocking(false)?;

            match proto::decode_client(&mut stream) {
                Ok(ClientMsg::Identify { .. }) => {}
                Ok(ClientMsg::ListSessions) => {
                    // Lightweight query: respond with session list and close.
                    let names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
                    let msg = proto::encode_server(&ServerMsg::SessionList { sessions: names });
                    let _ = proto::send(&mut stream, &msg);
                    return Ok(());
                }
                Ok(ClientMsg::KillServer) => {
                    crate::log::info("KillServer received, shutting down");
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "KillServer",
                    ));
                }
                _ => {
                    let _ = proto::send(
                        &mut stream,
                        &proto::encode_server(&ServerMsg::Error {
                            msg: "expected Identify or ListSessions".into(),
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

            // New client defaults to session 0, window 0.
            let session_idx = 0usize;
            let active = 0usize;
            if let Some(session) = sessions.get(session_idx)
                && let Some(window) = session.windows.get(active)
            {
                let pane = &window.pane;
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
            if let Some(session) = sessions.get(session_idx) {
                let status = proto::encode_server(&ServerMsg::StatusBarUpdate {
                    session: session.name.clone(),
                    windows: window_names(session),
                    active: active as u16,
                    session_count: sessions.len() as u16,
                });
                let _ = proto::send(&mut stream, &status);
            }

            let mut conn = ClientConn::new(stream);
            conn.session_idx = session_idx;
            conn.active_window = active;
            crate::log::info(&format!(
                "client connected: session_idx={session_idx}, window={active}, total clients={}",
                clients.len() + 1
            ));
            clients.push(conn);
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Send dirty rows + cursor to clients viewing a specific window in a specific session.
fn send_grid_update_to_window_viewers(
    clients: &mut Vec<ClientConn>,
    session_idx: usize,
    window_idx: usize,
    pane: &mut crate::server::pane::Pane,
) -> io::Result<()> {
    // Take pending scrollback rows and send them first.
    let scrolled = pane.take_pending_scrollback();
    if !scrolled.is_empty() {
        let sb_msg = proto::encode_server(&ServerMsg::ScrollbackUpdate { rows: scrolled });
        let mut i = 0;
        while i < clients.len() {
            if clients[i].session_idx == session_idx
                && clients[i].active_window == window_idx
                && proto::send(&mut clients[i].stream, &sb_msg).is_err()
            {
                clients.remove(i);
                continue;
            }
            i += 1;
        }
    }

    let dirty = pane.take_dirty_rows();
    let (cursor_row, cursor_col, cursor_visible) = pane.cursor();
    // Always send a GridUpdate when the PTY produced output, even if no
    // rows are dirty. The cursor may have moved (e.g., shell echoing a space
    // to an already-blank cell, or cursor-positioning escape sequences).
    // Without this, the client's cursor would not update until the next
    // dirty row — making it look like keypresses are ignored.
    let msg = proto::encode_server(&ServerMsg::GridUpdate {
        dirty,
        cursor_row,
        cursor_col,
        cursor_visible,
    });
    let mut i = 0;
    while i < clients.len() {
        if clients[i].session_idx == session_idx && clients[i].active_window == window_idx {
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
        0x0a => {
            // NewSession: 1 byte flag (0 = no name, 1 = has name) + optional name
            if data.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "NewSession needs at least 1 byte",
                ));
            }
            let has_name = data[0];
            if has_name != 0 {
                if data.len() < 5 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewSession name needs 4-byte length",
                    ));
                }
                let len = u32::from_le_bytes([data[1], data[2], data[3], data[4]]) as usize;
                if data.len() < 5 + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewSession name truncated",
                    ));
                }
                let name = String::from_utf8_lossy(&data[5..5 + len]).into_owned();
                ClientMsg::NewSession { name: Some(name) }
            } else {
                ClientMsg::NewSession { name: None }
            }
        }
        0x0b => ClientMsg::NextSession,
        0x0c => ClientMsg::PrevSession,
        0x0f => {
            // SelectSession: 4-byte length + name
            if data.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SelectSession needs 4-byte length",
                ));
            }
            let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if data.len() < 4 + len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SelectSession name truncated",
                ));
            }
            let name = String::from_utf8_lossy(&data[4..4 + len]).into_owned();
            ClientMsg::SelectSession { name }
        }
        0x0e => ClientMsg::KillSession,
        0x0d => ClientMsg::ListSessions,
        0x10 => ClientMsg::KillServer,
        0x11 => {
            // RenameSession: 4-byte length + name
            if data.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "RenameSession needs 4-byte length",
                ));
            }
            let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if data.len() < 4 + len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "RenameSession name truncated",
                ));
            }
            let name = String::from_utf8_lossy(&data[4..4 + len]).into_owned();
            ClientMsg::RenameSession { name }
        }
        0x12 => {
            // NewWindowIn: session (1-byte flag + optional name) + command (1-byte flag + optional string)
            if data.len() < 2 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "NewWindowIn needs at least 2 bytes",
                ));
            }
            let mut off = 0;
            let session = if data[off] != 0 {
                off += 1;
                if data.len() < off + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewWindowIn session name needs 4-byte length",
                    ));
                }
                let len =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                        as usize;
                off += 4;
                if data.len() < off + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewWindowIn session name truncated",
                    ));
                }
                let s = String::from_utf8_lossy(&data[off..off + len]).into_owned();
                off += len;
                Some(s)
            } else {
                off += 1;
                None
            };
            let command = if data.len() > off && data[off] != 0 {
                off += 1;
                if data.len() < off + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewWindowIn command needs 4-byte length",
                    ));
                }
                let len =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                        as usize;
                off += 4;
                if data.len() < off + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewWindowIn command truncated",
                    ));
                }
                let c = String::from_utf8_lossy(&data[off..off + len]).into_owned();
                Some(c)
            } else {
                None
            };
            ClientMsg::NewWindowIn { session, command }
        }
        0x13 => {
            // CaptureWindow: session (1-byte flag + optional name) + window (1-byte flag + optional u8)
            if data.len() < 2 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "CaptureWindow needs at least 2 bytes",
                ));
            }
            let mut off = 0;
            let session = if data[off] != 0 {
                off += 1;
                if data.len() < off + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "CaptureWindow session name needs 4-byte length",
                    ));
                }
                let len =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                        as usize;
                off += 4;
                if data.len() < off + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "CaptureWindow session name truncated",
                    ));
                }
                let s = String::from_utf8_lossy(&data[off..off + len]).into_owned();
                off += len;
                Some(s)
            } else {
                off += 1;
                None
            };
            let window = if data.len() > off && data[off] != 0 {
                off += 1;
                if data.len() < off + 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "CaptureWindow window index truncated",
                    ));
                }
                Some(data[off])
            } else {
                None
            };
            ClientMsg::CaptureWindow { session, window }
        }
        0x14 => {
            // SendKeys: session (1-byte flag + optional name) + window (1-byte flag + optional u8) + 4-byte length + keys
            if data.len() < 2 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SendKeys needs at least 2 bytes",
                ));
            }
            let mut off = 0;
            let session = if data[off] != 0 {
                off += 1;
                if data.len() < off + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "SendKeys session name needs 4-byte length",
                    ));
                }
                let len =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                        as usize;
                off += 4;
                if data.len() < off + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "SendKeys session name truncated",
                    ));
                }
                let s = String::from_utf8_lossy(&data[off..off + len]).into_owned();
                off += len;
                Some(s)
            } else {
                off += 1;
                None
            };
            let window = if data.len() > off && data[off] != 0 {
                off += 1;
                if data.len() < off + 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "SendKeys window index truncated",
                    ));
                }
                Some(data[off])
            } else {
                None
            };
            let keys = if let Some(start) = off.checked_add(1) {
                if data.len() < start + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "SendKeys needs 4-byte key length",
                    ));
                }
                let klen = u32::from_le_bytes([
                    data[start],
                    data[start + 1],
                    data[start + 2],
                    data[start + 3],
                ]) as usize;
                let kstart = start + 4;
                if data.len() < kstart + klen {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "SendKeys keys truncated",
                    ));
                }
                data[kstart..kstart + klen].to_vec()
            } else {
                Vec::new()
            };
            ClientMsg::SendKeys {
                session,
                window,
                keys,
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown client msg type: {msg_type}"),
            ));
        }
    };
    Ok(Some(msg))
}
