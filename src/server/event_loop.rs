// Server event loop: poll over listener, all window PTY fds, and all client sockets.
//
// Supports multiple sessions, multiple windows per session, multiple concurrent clients.
// Each client has its own active session and active window within that session.
// Grid updates are sent only to clients viewing the relevant window.
// A status bar (1 row) is reserved at the bottom of the client terminal.

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::fd::AsRawFd;

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};
use crate::pty;
use crate::server::session::Session;
use crate::server::state;
use crate::server::window::Window;

/// A connected client.
struct ClientConn {
    stream: crate::ipc::ConnStream,
    fd: i32,
    buf: Vec<u8>,
    /// Which session this client is attached to.
    session_idx: usize,
    /// This client's active window index within its session (per-client, not shared).
    active_window: usize,
    /// Whether this is an interactive (attached) client or a short-lived CLI client.
    attach: bool,
    /// Whether this is a control mode client (tmux -CC).
    /// Control clients receive text notifications instead of grid updates.
    is_control: bool,
    /// Buffered outbound data. Client sockets are non-blocking: when a
    /// client stops reading, sends accumulate here and are flushed on
    /// POLLOUT. If the buffer exceeds CLIENT_OUTBUF_CAP the client is
    /// disconnected — a stalled client must never freeze the event loop.
    outbuf: Vec<u8>,
    /// Command sequence counter for control-mode %begin/%end blocks.
    /// tmux numbers each command response; iTerm2 expects real values.
    control_seq: u64,
    /// Disconnect this client once its outbuf has fully drained.
    /// Used by control-mode `detach-client`: tmux exits the client after
    /// sending the response, so the lrmux -CC process can terminate and
    /// iTerm2 sees the detach.
    close_when_idle: bool,
    /// Backpressure: when output arrives faster than this client drains,
    /// incremental grid/scrollback updates are skipped once outbuf passes
    /// CLIENT_SUPPRESS_HIGH. The server grid is authoritative, so instead
    /// of disconnecting we freeze the client and send a fresh snapshot +
    /// paced scrollback replay once outbuf drains below CLIENT_SUPPRESS_LOW.
    /// Control clients stay suppressed until they send a command *after*
    /// draining — auto-resume re-floods iTerm2.
    suppressed: bool,
    /// Outer TTY default colors (OSC 10/11), probed by the client on attach.
    palette: super::capture::TerminalPalette,
    /// True when connected over TCP (plain or TLS). Unix clients skip
    /// auth_token checks (filesystem permissions are the gate).
    is_tcp: bool,
}

/// Maximum bytes buffered for a slow client before disconnecting it.
const CLIENT_OUTBUF_CAP: usize = 8 * 1024 * 1024;
/// Outbuf level at which incremental updates to a normal client are
/// suspended (well below the hard cap — the client freezes instead of
/// being disconnected).
const CLIENT_SUPPRESS_HIGH: usize = 2 * 1024 * 1024;
/// Outbuf level at which a suppressed client is resynced with a snapshot.
const CLIENT_SUPPRESS_LOW: usize = 256 * 1024;

impl ClientConn {
    fn new(stream: crate::ipc::ConnStream, attach: bool) -> Self {
        let fd = stream.as_raw_fd();
        let is_tcp = stream.is_tcp();
        Self {
            stream,
            fd,
            buf: Vec::new(),
            session_idx: 0,
            active_window: 0,
            attach,
            is_control: false,
            outbuf: Vec::new(),
            control_seq: 0,
            close_when_idle: false,
            suppressed: false,
            palette: super::capture::TerminalPalette::default(),
            is_tcp,
        }
    }
}

/// TCP clients must present a matching auth_token when the server has one.
fn check_tcp_auth(client: &ClientConn, token: &str) -> bool {
    tcp_auth_ok(client.is_tcp, token)
}

/// Queue bytes for a client and flush what can be written without blocking.
/// Returns false if the client should be disconnected (write error, or the
/// buffer would grow past CLIENT_OUTBUF_CAP because the client stopped reading).
fn client_send(client: &mut ClientConn, bytes: &[u8]) -> bool {
    // Refuse before extending — otherwise a single oversized enqueue (e.g.
    // scrollback replay after a burst) trips the cap and kills the client.
    if client.outbuf.len().saturating_add(bytes.len()) > CLIENT_OUTBUF_CAP {
        crate::log::warn(&format!(
            "client fd {} outbuf exceeded {CLIENT_OUTBUF_CAP} bytes, disconnecting",
            client.fd
        ));
        return false;
    }
    client.outbuf.extend_from_slice(bytes);
    flush_client_outbuf(client)
}

/// Write as much buffered data to the client socket as possible without
/// blocking. Returns false on write error (client should be disconnected).
/// Leftover data stays in outbuf and is retried when POLLOUT fires.
fn flush_client_outbuf(client: &mut ClientConn) -> bool {
    while !client.outbuf.is_empty() {
        let n = unsafe {
            libc::send(
                client.fd,
                client.outbuf.as_ptr() as *const _,
                client.outbuf.len(),
                0,
            )
        };
        if n > 0 {
            client.outbuf.drain(..n as usize);
        } else if n == 0 {
            return false;
        } else {
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return true,
                _ => return false,
            }
        }
    }
    true
}

/// Run the server event loop.
///
/// Phase 4: multiple sessions, multiple windows, multiple clients, prefix-key commands.
/// Each client has its own active session and active window. The server persists until
/// all sessions are closed.
pub fn run(
    listeners: Vec<crate::ipc::ConnListener>,
    socket_path: &std::path::Path,
    headless: bool,
    discovery_sock: Option<std::net::UdpSocket>,
) -> io::Result<()> {
    // Capture once at startup. Probes (ListSessions) and failed first
    // handshakes must not consume / lose `new-server -- cmd` options.
    let boot = take_bootstrap();

    let (mut grid_rows, mut grid_cols, mut sessions, mut clients) = if headless {
        // Headless mode: create a default session (24x80) without waiting
        // for the first client. Used by `lrmux new-server --headless`
        // (`start-server` is the same) for testing and remote management.
        let session_name = boot.session_name.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "session".to_string())
        });
        let session = make_bootstrap_session(session_name, 24, 80, &boot);
        crate::log::info("server started in headless mode (24x80)");
        (24u16, 80u16, vec![session], vec![])
    } else {
        // Normal mode: wait for the first client to determine terminal size.
        // Retry on bad connections (e.g. ListSessions queries from the selector,
        // or connections that send unexpected data).
        loop {
            // Accept from any listener (Unix or TCP).
            let stream = accept_from_any(&listeners)?;
            match handshake_first_client(stream, &boot) {
                Ok(result) => break result,
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
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
        }
    };

    // Set all listeners to non-blocking so we can poll them alongside clients.
    for l in &listeners {
        l.set_nonblocking(true)?;
    }
    let listener_fds: Vec<i32> = listeners.iter().map(|l| l.as_raw_fd()).collect();

    // Client sockets are non-blocking in the main loop: a slow or hung
    // client must never stall the event loop. Outbound data goes through
    // each client's outbuf and is flushed on POLLOUT.
    for c in &mut clients {
        let _ = c.stream.set_nonblocking(true);
    }

    // Periodic state save counter (save every ~1000 iterations ≈ 10s).
    let mut iter_count: u32 = 0;

    // Set by control-mode `kill-server` — break out of the loop and run
    // the normal graceful shutdown (SIGHUP children, save state, cleanup).
    let mut shutdown = false;

    // TCP listen port for discovery Announce (0 if no TCP).
    let announce_tcp_port: u16 = listeners
        .iter()
        .find_map(|l| match l {
            crate::ipc::ConnListener::Tcp(t) => t.local_addr().ok().map(|a| a.port()),
            _ => None,
        })
        .unwrap_or(0);
    let announce_tls = crate::server::tls_server_config().is_some()
        && !matches!(crate::server::network().tls, crate::config::TlsMode::Off);

    loop {
        // Build pollfd array: listeners + discovery + all window PTY fds + all client fds.
        let mut pty_map: Vec<(usize, usize)> = Vec::new();
        let mut fds = Vec::with_capacity(listener_fds.len() + 64 + clients.len() + 1);
        for &lfd in &listener_fds {
            fds.push(libc::pollfd {
                fd: lfd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let discovery_idx = if let Some(ref ds) = discovery_sock {
            fds.push(libc::pollfd {
                fd: ds.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            Some(fds.len() - 1)
        } else {
            None
        };
        for (si, session) in sessions.iter().enumerate() {
            for (wi, w) in session.windows.iter().enumerate() {
                if w.pane.is_exited() {
                    continue;
                }
                pty_map.push((si, wi));
                let pending = !w.pane.pending_input.is_empty();
                fds.push(libc::pollfd {
                    fd: w.pty_fd(),
                    events: libc::POLLIN | if pending { libc::POLLOUT } else { 0 },
                    revents: 0,
                });
            }
        }
        let num_pty_fds = pty_map.len();
        let pty_base = listener_fds.len() + if discovery_idx.is_some() { 1 } else { 0 };
        for c in &clients {
            fds.push(libc::pollfd {
                fd: c.fd,
                // POLLOUT is needed while outbuf has pending data, and also
                // for suppressed clients — the level-triggered writable
                // event is what triggers their snapshot resync.
                events: libc::POLLIN
                    | if c.outbuf.is_empty() && !c.suppressed {
                        0
                    } else {
                        libc::POLLOUT
                    },
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

        // Listeners readable → accept new client from any listener.
        for (li, _lfd) in listener_fds.iter().enumerate() {
            if fds[li].revents & libc::POLLIN != 0 {
                match accept_new_client(
                    &listeners[li],
                    &sessions,
                    grid_rows,
                    grid_cols,
                    &mut clients,
                ) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                        // KillServer received — shut down gracefully.
                        crate::log::info("KillServer received, shutting down");
                        kill_all_children(&sessions);
                        state::save_state(socket_path, &sessions);
                        broadcast_to_all(
                            &mut clients,
                            &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                        );
                        shutdown_flush(&mut clients);
                        ipc::cleanup(socket_path);
                        return Ok(());
                    }
                    Err(_) => {}
                }
            }
        }

        // Discovery UDP: respond to Discover probes with Announce.
        if let (Some(di), Some(ds)) = (discovery_idx, discovery_sock.as_ref())
            && fds[di].revents & libc::POLLIN != 0
            && announce_tcp_port != 0
        {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = ds.recv_from(&mut buf) {
                if let Some(crate::ipc::discovery::ParsedPacket::Discover) =
                    crate::ipc::discovery::parse_packet(&buf[..n], from)
                {
                    let pkt = crate::ipc::discovery::encode_announce(
                        crate::server::server_name(),
                        announce_tcp_port,
                        announce_tls,
                        crate::version::VERSION,
                        crate::server::tls_fingerprint(),
                    );
                    let _ = ds.send_to(&pkt, from);
                }
            }
        }

        // Window PTY output → grid → send only to clients viewing that window.
        // First pass: read all PTYs, collect grid updates and child exits.
        // We must NOT modify sessions/windows during this pass because
        // pty_map indices would become stale for subsequent PTYs.
        let mut pty_exits: Vec<(usize, usize, i32)> = Vec::new();
        for pty_i in 0..num_pty_fds {
            let pf = &fds[pty_base + pty_i];
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
                    Ok((true, raw, osc_queries)) => {
                        let pane_id = pane.id;
                        let _ = send_grid_update_to_window_viewers(&mut clients, si, wi, pane);
                        // Forward raw output to control clients viewing this window.
                        let cc_bytes = pane.take_cc_forward_bytes(&raw);
                        if !cc_bytes.is_empty() {
                            forward_output_to_control_clients(&mut clients, pane_id, &cc_bytes);
                        }
                        // Proxy OSC 10/11 color queries to a real attached TTY.
                        for q in osc_queries {
                            proxy_osc_color_query(&mut clients, si, wi, pane_id, &q);
                        }
                    }
                    Ok((false, raw, osc_queries)) => {
                        let pane_id = pane.id;
                        // Child exited — forward any remaining bytes (including
                        // a flushed incomplete UTF-8 tail) before reaping.
                        let mut cc_bytes = pane.take_cc_forward_bytes(&raw);
                        cc_bytes.extend(pane.flush_cc_forward_bytes());
                        if !cc_bytes.is_empty() {
                            forward_output_to_control_clients(&mut clients, pane_id, &cc_bytes);
                        }
                        for q in osc_queries {
                            proxy_osc_color_query(&mut clients, si, wi, pane_id, &q);
                        }
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

        // Drain pending stdin for PTYs that are now writable. This completes
        // writes that couldn't be done in one shot (e.g., a burst of arrow keys
        // while the child is busy), keeping escape sequences intact.
        for pty_i in 0..num_pty_fds {
            let pf = &fds[pty_base + pty_i];
            if pf.revents & libc::POLLOUT != 0 {
                let (si, wi) = pty_map[pty_i];
                if si < sessions.len() && wi < sessions[si].windows.len() {
                    let _ = sessions[si].windows[wi].pane.flush_pending_input();
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
            // as success — auto-close the window. When a control client
            // (iTerm2) is attached to this session, use tmux semantics:
            // the window closes on ANY exit code (remain-on-exit off).
            let control_attached = clients.iter().any(|c| c.is_control && c.session_idx == si);
            if exit_code == 0 || exit_code == 130 || exit_code == -2 || control_attached {
                // Exit code 0: auto-close the window.
                let wid = session.windows[wi].id_str();
                session.windows.remove(wi);
                // Tell control clients (iTerm2) the window is gone.
                broadcast_control_notify(&mut clients, &format!("%window-close {}", wid));
                if session.windows.is_empty() {
                    // Last window in this session closed — remove the session.
                    crate::log::info(&format!(
                        "last window in session '{session_name}' closed, removing session"
                    ));
                    sessions.remove(si);
                    broadcast_control_notify(&mut clients, "%sessions-changed");
                    if sessions.is_empty() {
                        // Last session closed — shut down the server.
                        crate::log::info("last session closed, shutting down server");
                        broadcast_to_all(
                            &mut clients,
                            &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                        );
                        shutdown_flush(&mut clients);
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
            // Clients may have been removed during PTY processing (write
            // failures). Bounds-check before indexing.
            if client_idx >= clients.len() {
                break;
            }
            let pf = &fds[pty_base + num_pty_fds + client_idx];
            crate::log::debug(&format!(
                "client {} poll revents=0x{:x} fd={}",
                client_idx, pf.revents, pf.fd
            ));
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
                    crate::log::debug(&format!("read {} bytes from client {}", n, client_idx));
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
                                            let pane = &mut sessions[si].windows[aw].pane;
                                            let translated = translate_cursor_keys(
                                                &data,
                                                pane.grid.app_cursor_keys,
                                            );
                                            let _ = pane.write_input(&translated);
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
                                    ClientMsg::Refresh => {
                                        // Ctrl-A r: force a full resync of the current view
                                        // and exit high-output suppression for this client.
                                        let ci = client_idx;
                                        clients[ci].suppressed = false;
                                        if !need_snapshot.contains(&ci) {
                                            need_snapshot.push(ci);
                                        }
                                        need_status_bar_all = true;
                                        crate::log::info(&format!(
                                            "client fd {} requested refresh",
                                            clients[ci].fd
                                        ));
                                    }
                                    ClientMsg::TermOscReply { pane_id, data } => {
                                        // Real TTY answered OSC 10/11 — cache palette + inject.
                                        if let Some((si, wi)) = find_pane_by_id(&sessions, pane_id)
                                        {
                                            sessions[si].windows[wi]
                                                .pane
                                                .note_osc_color_reply(&data);
                                            let _ =
                                                sessions[si].windows[wi].pane.write_input(&data);
                                        }
                                    }
                                    ClientMsg::TermPalette { fg, bg } => {
                                        let pal = super::capture::TerminalPalette { fg, bg };
                                        clients[client_idx].palette = pal;
                                        seed_pane_palette(
                                            &mut sessions,
                                            clients[client_idx].session_idx,
                                            clients[client_idx].active_window,
                                            pal,
                                        );
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
                                            seed_client_focus_palette(
                                                &mut sessions,
                                                &clients,
                                                client_idx,
                                            );
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
                                            seed_client_focus_palette(
                                                &mut sessions,
                                                &clients,
                                                client_idx,
                                            );
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
                                            seed_client_focus_palette(
                                                &mut sessions,
                                                &clients,
                                                client_idx,
                                            );
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
                                            seed_client_focus_palette(
                                                &mut sessions,
                                                &clients,
                                                client_idx,
                                            );
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
                                                shutdown_flush(&mut clients);
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
                                    ClientMsg::NewSession { name, cwd, command } => {
                                        // Use the CWD from the client message if provided
                                        // (e.g. from `lrmux new-session` CLI). Otherwise,
                                        // fall back to the CWD of the active window's child.
                                        let si = clients[client_idx].session_idx;
                                        let wi = clients[client_idx].active_window;
                                        let cwd_full = cwd.or_else(|| {
                                            if si < sessions.len()
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
                                            }
                                        });
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
                                        // If a command was specified, replace the
                                        // default shell window with a window running
                                        // that command.
                                        if let Some(ref cmd) = command {
                                            let win = Window::new_with_command(
                                                grid_rows,
                                                grid_cols,
                                                default_window_name(),
                                                cmd,
                                                cwd_full.as_deref(),
                                            );
                                            // Replace the first window (default shell)
                                            // with the command window.
                                            sessions[new_si].windows[0] = win;
                                        }
                                        // Switch the sending client to the new session.
                                        if clients[client_idx].attach {
                                            clients[client_idx].session_idx = new_si;
                                            clients[client_idx].active_window = 0;
                                            if !need_snapshot.contains(&client_idx) {
                                                need_snapshot.push(client_idx);
                                            }
                                        }
                                        // Also switch all other attached clients
                                        // (auto-switch to new session).
                                        for (ci, c) in clients.iter_mut().enumerate() {
                                            if ci != client_idx
                                                && c.attach
                                                && !need_snapshot.contains(&ci)
                                            {
                                                c.session_idx = new_si;
                                                c.active_window = 0;
                                                need_snapshot.push(ci);
                                            }
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
                                            if clients[client_idx].attach {
                                                // Interactive client: switch just this one.
                                                clients[client_idx].session_idx = idx;
                                                clients[client_idx].active_window = 0;
                                                if !need_snapshot.contains(&client_idx) {
                                                    need_snapshot.push(client_idx);
                                                }
                                            } else {
                                                // CLI client: switch all attached clients.
                                                for (ci, c) in clients.iter_mut().enumerate() {
                                                    if c.attach {
                                                        c.session_idx = idx;
                                                        c.active_window = 0;
                                                        if !need_snapshot.contains(&ci) {
                                                            need_snapshot.push(ci);
                                                        }
                                                    }
                                                }
                                            }
                                            need_status_bar_all = true;
                                        } else {
                                            crate::log::warn(&format!(
                                                "SelectSession: session '{name}' not found"
                                            ));
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
                                            shutdown_flush(&mut clients);
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
                                            address: crate::server::server_address().to_string(),
                                        });
                                        let _ = client_send(&mut clients[client_idx], &msg);
                                    }
                                    ClientMsg::KillServer => {
                                        eprintln!("lrmux: KillServer received, shutting down.");
                                        broadcast_to_all(
                                            &mut clients,
                                            &proto::encode_server(&ServerMsg::PaneExit { code: 0 }),
                                        );
                                        shutdown_flush(&mut clients);
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
                                                    None,
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
                                    ClientMsg::CaptureWindow {
                                        session,
                                        window,
                                        format,
                                        colors,
                                        term_fg,
                                        term_bg,
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
                                        let fmt = super::capture::CaptureFormat::from_u8(format);
                                        let content = if let Some(si) = si {
                                            let wi = match window {
                                                Some(w) => Some(w as usize),
                                                None => Some(clients[client_idx].active_window),
                                            };
                                            if let Some(wi) = wi
                                                && wi < sessions[si].windows.len()
                                            {
                                                let palette = resolve_capture_palette(
                                                    &sessions, &clients, si, wi, term_fg, term_bg,
                                                );
                                                Some(super::capture::render_pane(
                                                    &sessions[si].windows[wi].pane,
                                                    fmt,
                                                    colors,
                                                    false,
                                                    palette,
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
                                        let _ = client_send(&mut clients[client_idx], &msg);
                                    }
                                    ClientMsg::SendKeys {
                                        session,
                                        window,
                                        keys,
                                    } => {
                                        crate::log::info(&format!(
                                            "SendKeys: session={:?} window={:?} keys_len={}",
                                            session,
                                            window,
                                            keys.len()
                                        ));
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
                                                crate::log::info(&format!(
                                                    "SendKeys: writing {} bytes to session '{}' window {}",
                                                    keys.len(),
                                                    sessions[si].name,
                                                    wi
                                                ));
                                                let translated = translate_cursor_keys(
                                                    &keys,
                                                    sessions[si].windows[wi]
                                                        .pane
                                                        .grid
                                                        .app_cursor_keys,
                                                );
                                                let _ = sessions[si].windows[wi]
                                                    .pane
                                                    .write_input(&translated);
                                            } else {
                                                crate::log::warn(&format!(
                                                    "SendKeys: window index {} out of range (session '{}' has {} windows)",
                                                    wi.unwrap_or(0),
                                                    sessions[si].name,
                                                    sessions[si].windows.len()
                                                ));
                                            }
                                        } else {
                                            crate::log::warn(&format!(
                                                "SendKeys: session {:?} not found",
                                                session
                                            ));
                                        }
                                    }
                                    ClientMsg::GetLog => {
                                        let lines = crate::log::get_ring_log();
                                        let msg =
                                            proto::encode_server(&ServerMsg::LogContent { lines });
                                        let _ = client_send(&mut clients[client_idx], &msg);
                                    }
                                    ClientMsg::SetPsk { psk } => {
                                        if !clients[client_idx].attach
                                            && !clients[client_idx].is_control
                                        {
                                            let msg = proto::encode_server(&ServerMsg::Error {
                                                msg: "SetPsk requires an attached client".into(),
                                            });
                                            let _ = client_send(&mut clients[client_idx], &msg);
                                        } else {
                                            crate::server::set_runtime_psk(psk.clone());
                                            if let Err(e) = crate::config::persist_psk(&psk) {
                                                crate::log::warn(&format!(
                                                    "failed to persist PSK to config: {e}"
                                                ));
                                            }
                                            crate::log::info("PSK updated via SetPsk");
                                            let msg = proto::encode_server(&ServerMsg::PskUpdated);
                                            let _ = client_send(&mut clients[client_idx], &msg);
                                        }
                                    }
                                    ClientMsg::IdentifyControl {
                                        rows,
                                        cols,
                                        auth_token,
                                    } => {
                                        if !check_tcp_auth(&clients[client_idx], &auth_token) {
                                            let msg = proto::encode_server(&ServerMsg::Error {
                                                msg: "authentication failed".into(),
                                            });
                                            let _ = client_send(&mut clients[client_idx], &msg);
                                            to_remove.push(client_idx);
                                            break;
                                        }
                                        // Control mode client: mark as control
                                        // and emit the initial state right away,
                                        // like real tmux -CC does after the DCS.
                                        clients[client_idx].is_control = true;
                                        clients[client_idx].attach = false;
                                        // Send IdentifyAck with the requested grid size.
                                        let msg = proto::encode_server(&ServerMsg::IdentifyAck {
                                            rows,
                                            cols,
                                            version: crate::version::VERSION.to_string(),
                                            address: crate::server::server_address().to_string(),
                                        });
                                        let _ = client_send(&mut clients[client_idx], &msg);
                                        send_control_initial_state(
                                            &mut clients[client_idx],
                                            &sessions,
                                        );
                                    }
                                    ClientMsg::ControlCommand { line } => {
                                        // Resume fast-forward only once the client
                                        // has drained enough that a new flood won't
                                        // immediately re-trip the cap. Clearing on
                                        // every select-pane/send while still behind
                                        // oscillates and freezes iTerm2.
                                        if clients[client_idx].suppressed
                                            && clients[client_idx].outbuf.len()
                                                <= CLIENT_SUPPRESS_LOW
                                        {
                                            clients[client_idx].suppressed = false;
                                            crate::log::info(&format!(
                                                "control client fd {} resumed after fast-forward",
                                                clients[client_idx].fd
                                            ));
                                            send_control_notify(
                                                &mut clients[client_idx],
                                                "%message lrmux: output resumed",
                                            );
                                        }
                                        // Parse and execute a tmux-style command.
                                        let mut pending_affinities = None;
                                        if handle_control_command(
                                            &mut clients[client_idx],
                                            &mut sessions,
                                            &line,
                                            socket_path,
                                            &mut pending_affinities,
                                        ) {
                                            shutdown = true;
                                        }
                                        if let Some((si, value)) = pending_affinities
                                            && migrate_affinity_sessions(
                                                &mut sessions,
                                                si,
                                                &value,
                                                &mut clients,
                                            )
                                        {
                                            broadcast_control_notify(
                                                &mut clients,
                                                "%sessions-changed",
                                            );
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
                // On POLLHUP, try one more read — the peer may have sent data
                // before closing (e.g. CLI commands that send a message and exit).
                let mut buf = [0u8; 8192];
                let n = unsafe {
                    libc::read(
                        clients[client_idx].fd,
                        buf.as_mut_ptr() as *mut _,
                        buf.len(),
                    )
                };
                if n > 0 {
                    crate::log::debug(&format!(
                        "read {} bytes from client {} on POLLHUP",
                        n, client_idx
                    ));
                    clients[client_idx]
                        .buf
                        .extend_from_slice(&buf[..n as usize]);
                    // Process any complete frames before removing the client.
                    while let Ok(Some(msg)) = try_parse_frame(&mut clients[client_idx].buf) {
                        crate::log::debug(&format!(
                            "client {} POLLHUP frame processed: {:?}",
                            client_idx, msg
                        ));
                        // Handle simple non-attach messages that don't need
                        // the full client context.
                        match msg {
                            ClientMsg::NewWindowIn { session, command } => {
                                let si = match session {
                                    Some(ref name) => sessions.iter().position(|s| &s.name == name),
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
                                            None,
                                        ),
                                        None => {
                                            Window::new(grid_rows, grid_cols, default_window_name())
                                        }
                                    };
                                    sessions[si].windows.push(win);
                                    need_status_bar_all = true;
                                    crate::log::info(&format!(
                                        "new window {} in session '{}' (from POLLHUP)",
                                        sessions[si].windows.len() - 1,
                                        sessions[si].name
                                    ));
                                }
                            }
                            ClientMsg::NewSession { name, cwd, command } => {
                                let cwd_full = cwd.or_else(|| {
                                    let si = clients[client_idx].session_idx;
                                    let wi = clients[client_idx].active_window;
                                    if si < sessions.len() && wi < sessions[si].windows.len() {
                                        let pane = &sessions[si].windows[wi].pane;
                                        if !pane.exited {
                                            pty::child_cwd_full(pane.pty.child_pid)
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                });
                                let session_name = name.unwrap_or_else(|| match &cwd_full {
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
                                    Some(cwd) => {
                                        Session::new_in_cwd(session_name, grid_rows, grid_cols, cwd)
                                    }
                                    None => Session::new(session_name, grid_rows, grid_cols),
                                };
                                sessions.push(session);
                                let new_si = sessions.len() - 1;
                                // If a command was specified, replace the default
                                // shell window with a command window.
                                if let Some(ref cmd) = command {
                                    let win = Window::new_with_command(
                                        grid_rows,
                                        grid_cols,
                                        default_window_name(),
                                        cmd,
                                        cwd_full.as_deref(),
                                    );
                                    sessions[new_si].windows[0] = win;
                                }
                                // Auto-switch all attached clients to the new session.
                                for c in &mut *clients {
                                    if c.attach {
                                        c.session_idx = new_si;
                                        c.active_window = 0;
                                    }
                                }
                                need_status_bar_all = true;
                                crate::log::info(&format!(
                                    "new session '{}' created (index {}, from POLLHUP)",
                                    sessions.last().unwrap().name,
                                    new_si
                                ));
                            }
                            ClientMsg::SendKeys {
                                session,
                                window,
                                keys,
                            } => {
                                crate::log::info(&format!(
                                    "SendKeys: session={:?} window={:?} keys_len={} (from POLLHUP)",
                                    session,
                                    window,
                                    keys.len()
                                ));
                                let si = match session {
                                    Some(ref name) => sessions.iter().position(|s| &s.name == name),
                                    None => Some(0),
                                };
                                if let Some(si) = si
                                    && let Some(session) = sessions.get_mut(si)
                                {
                                    let wi = match window {
                                        Some(w) => Some(w as usize),
                                        None => Some(clients[client_idx].active_window),
                                    };
                                    if let Some(wi) = wi
                                        && let Some(window) = session.windows.get_mut(wi)
                                    {
                                        let translated = translate_cursor_keys(
                                            &keys,
                                            window.pane.grid.app_cursor_keys,
                                        );
                                        let _ = window.pane.write_input(&translated);
                                        crate::log::info(&format!(
                                            "SendKeys: writing {} bytes to session '{}' window {}",
                                            translated.len(),
                                            session.name,
                                            wi
                                        ));
                                    }
                                }
                            }
                            ClientMsg::SelectSession { name } => {
                                if let Some(idx) = sessions.iter().position(|s| s.name == name) {
                                    for c in &mut *clients {
                                        if c.attach {
                                            c.session_idx = idx;
                                            c.active_window = 0;
                                        }
                                    }
                                    need_status_bar_all = true;
                                    crate::log::info(&format!(
                                        "SelectSession: switched to '{name}' (from POLLHUP)"
                                    ));
                                } else {
                                    crate::log::warn(&format!(
                                        "SelectSession: session '{name}' not found (from POLLHUP)"
                                    ));
                                }
                            }
                            _ => {
                                // Other messages are ignored on POLLHUP.
                            }
                        }
                    }
                }
                to_remove.push(client_idx);
            }
            // Client socket writable → drain its outbound buffer.
            if pf.revents & libc::POLLOUT != 0
                && client_idx < clients.len()
                && !to_remove.contains(&client_idx)
            {
                if !flush_client_outbuf(&mut clients[client_idx]) {
                    to_remove.push(client_idx);
                } else if clients[client_idx].suppressed
                    && !clients[client_idx].is_control
                    && clients[client_idx].outbuf.len() <= CLIENT_SUPPRESS_LOW
                {
                    // Interactive client caught up — resync with a fresh
                    // snapshot. Control clients stay suppressed until they
                    // send a command after draining (auto-resume re-floods
                    // iTerm2 and freezes the -CC session).
                    clients[client_idx].suppressed = false;
                    crate::log::info(&format!(
                        "client fd {} caught up after output burst, resyncing",
                        clients[client_idx].fd
                    ));
                    if !need_snapshot.contains(&client_idx) {
                        need_snapshot.push(client_idx);
                    }
                }
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

        // Clients flagged close_when_idle (control-mode detach) are removed
        // once their outbound buffer has fully drained.
        for (ci, c) in clients.iter().enumerate() {
            if c.close_when_idle && c.outbuf.is_empty() && !to_remove.contains(&ci) {
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

        // If no sessions and no clients, or kill-server was requested, exit.
        if shutdown || (sessions.is_empty() && clients.is_empty()) {
            break;
        }

        // Periodic state save (every ~1000 iterations ≈ 10s).
        iter_count = iter_count.wrapping_add(1);
        if iter_count.is_multiple_of(1000) && !sessions.is_empty() {
            state::save_state(socket_path, &sessions);
        }
    }

    crate::log::info("no sessions and no clients remaining, server exiting");
    // Send SIGHUP to all remaining child processes before cleanup.
    kill_all_children(&sessions);
    state::save_state(socket_path, &sessions);
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

/// Translate normal cursor keys to application cursor keys when the mode is active.
/// When `app_cursor_keys` is true:
/// - \x1b[A/B/C/D → \x1bOA/B/C/D (arrow keys)
/// - \x1b[H → \x1bOH (Home)
/// - \x1b[F → \x1bOF (End)
///
/// This handles the DECCKM (cursor key mode) that programs like htop and zsh enable.
fn translate_cursor_keys(data: &[u8], app_cursor_keys: bool) -> Vec<u8> {
    if !app_cursor_keys {
        return data.to_vec();
    }
    // Look for \x1b[A, \x1b[B, \x1b[C, \x1b[D (arrows) and \x1b[H, \x1b[F (Home/End).
    // Translate the intermediate '[' to 'O' when in application cursor keys mode.
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len()
            && data[i] == 0x1b
            && data[i + 1] == b'['
            && matches!(data[i + 2], b'A' | b'B' | b'C' | b'D' | b'H' | b'F')
        {
            result.push(0x1b);
            result.push(b'O');
            result.push(data[i + 2]);
            i += 3;
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
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

/// Bootstrap options passed from the parent via env before `fork_server`.
struct Bootstrap {
    command: Option<String>,
    session_name: Option<String>,
    cwd: Option<String>,
}

/// Read and clear `LRMUX_INIT_*` so a later pane spawn does not see them.
fn take_bootstrap() -> Bootstrap {
    let command = std::env::var("LRMUX_INIT_COMMAND")
        .ok()
        .filter(|s| !s.is_empty());
    let session_name = std::env::var("LRMUX_INIT_SESSION")
        .ok()
        .filter(|s| !s.is_empty());
    let cwd = std::env::var("LRMUX_INIT_CWD")
        .ok()
        .filter(|s| !s.is_empty());
    // Safety: server is single-threaded at startup.
    unsafe {
        std::env::remove_var("LRMUX_INIT_COMMAND");
        std::env::remove_var("LRMUX_INIT_SESSION");
        std::env::remove_var("LRMUX_INIT_CWD");
    }
    Bootstrap {
        command,
        session_name,
        cwd,
    }
}

fn make_bootstrap_session(name: String, rows: u16, cols: u16, boot: &Bootstrap) -> Session {
    // `new-server -- cmd` uses `$SHELL -ci …` (see Pane::new_with_command) so
    // interactive rc files load — plain `-c` skips `.zshrc` and breaks
    // truecolor for tools like vim. No command → default interactive shell.
    match (&boot.command, &boot.cwd) {
        (Some(cmd), cwd) => Session::new_with_command(name, rows, cols, cmd, cwd.as_deref()),
        (None, Some(cwd)) => Session::new_in_cwd(name, rows, cols, cwd),
        (None, None) => Session::new(name, rows, cols),
    }
}

/// Render a pane's grid as plain text (for capture-window).
/// Each row is trimmed of trailing whitespace and joined with newlines.
fn render_grid_text(pane: &crate::server::pane::Pane) -> String {
    render_pane_text(pane, false, false)
}

/// Render a pane (optionally including scrollback) as text.
/// `with_escapes` (`capture-pane -e`) emits SGR sequences preserving
/// colors and attributes; `include_scrollback` (`capture-pane -S -<n>`)
/// prepends the scrollback history so iTerm2 can rebuild its buffer.
fn render_pane_text(
    pane: &crate::server::pane::Pane,
    with_escapes: bool,
    include_scrollback: bool,
) -> String {
    let rows = pane.rows as usize;
    let cols = pane.cols as usize;
    let scrollback = if include_scrollback {
        pane.scrollback_rows()
    } else {
        Vec::new()
    };
    let mut lines = Vec::with_capacity(scrollback.len() + rows);
    fn emit_row(
        lines: &mut Vec<String>,
        r: &[crate::grid::cell::Cell],
        cols: usize,
        with_escapes: bool,
    ) {
        let mut line = String::with_capacity(cols);
        let mut last: Option<(
            crate::grid::cell::Color,
            crate::grid::cell::Color,
            crate::grid::cell::Attr,
        )> = None;
        let mut styled = false;
        for c in r.iter().take(cols) {
            if with_escapes && last != Some((c.fg, c.bg, c.attrs)) {
                crate::client::render::emit_sgr(&mut line, c.fg, c.bg, c.attrs);
                last = Some((c.fg, c.bg, c.attrs));
                styled = true;
            }
            line.push(if c.ch == '\0' { ' ' } else { c.ch });
        }
        let mut line = line.trim_end().to_string();
        if styled {
            line.push_str("\x1b[0m");
        }
        lines.push(line);
    }
    for r in &scrollback {
        emit_row(&mut lines, r, cols, with_escapes);
    }
    for row in 0..rows {
        if let Some(r) = pane.grid.row(row) {
            emit_row(&mut lines, r, cols, with_escapes);
        } else {
            lines.push(String::new());
        }
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
        if si >= sessions.len() || clients[i].is_control {
            i += 1;
            continue;
        }
        let msg = proto::encode_server(&ServerMsg::StatusBarUpdate {
            session: sessions[si].name.clone(),
            windows: window_names(&sessions[si]),
            active: clients[i].active_window as u16,
            session_count: sessions.len() as u16,
            high_output: clients[i].suppressed,
        });
        if !client_send(&mut clients[i], &msg) {
            clients.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Send a status bar update to a single client (using its session + active window).
fn send_status_bar_to_client(client: &mut ClientConn, sessions: &[Session]) {
    let si = client.session_idx;
    if si >= sessions.len() || client.is_control {
        return;
    }
    let msg = proto::encode_server(&ServerMsg::StatusBarUpdate {
        session: sessions[si].name.clone(),
        windows: window_names(&sessions[si]),
        active: client.active_window as u16,
        session_count: sessions.len() as u16,
        high_output: client.suppressed,
    });
    let _ = client_send(client, &msg);
}

/// Send a full grid snapshot of a client's active window to that client.
fn send_snapshot_to_client(client: &mut ClientConn, sessions: &[Session]) -> io::Result<()> {
    let si = client.session_idx;
    let aw = client.active_window;
    if client.is_control {
        // Control clients don't consume binary grid frames.
        return Ok(());
    }
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
    if !client_send(client, &snapshot) {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "client gone",
        ));
    }

    // Then send scrollback so the client can populate the fresh grid's
    // history. Chunk it and pace against outbuf — a full 10k-row buffer at
    // wide sizes encodes to tens of MB. Prefer recent history when the
    // budget can't fit everything (oldest rows are dropped first).
    let sb_rows = pane.scrollback_rows();
    if !sb_rows.is_empty() && !send_scrollback_replay_paced(client, &sb_rows) {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "client gone",
        ));
    }
    Ok(())
}

/// Best-effort scrollback replay that never pushes a client to the hard
/// outbuf cap. A fast client (outbuf drains each send) gets the full
/// history; a slow/post-burst client gets recent history only.
fn send_scrollback_replay_paced(
    client: &mut ClientConn,
    sb_rows: &[Vec<crate::grid::Cell>],
) -> bool {
    // When already behind (typical after burst resync), prefer the newest
    // rows that fit under CLIENT_SUPPRESS_HIGH. Quiet attaches start at 0
    // and stream the full buffer while the client keeps up.
    let start = if client.outbuf.len() > CLIENT_SUPPRESS_LOW {
        let cols = sb_rows.first().map(|r| r.len()).unwrap_or(80).max(1);
        let approx_row = cols * 10;
        let room = CLIENT_SUPPRESS_HIGH.saturating_sub(client.outbuf.len());
        let max_rows = (room / approx_row).max(1);
        let s = sb_rows.len().saturating_sub(max_rows);
        if s > 0 {
            crate::log::info(&format!(
                "client fd {}: scrollback replay truncated (sending newest {}/{} rows)",
                client.fd,
                sb_rows.len() - s,
                sb_rows.len()
            ));
        }
        s
    } else {
        0
    };

    let mut sent = 0usize;
    for chunk in sb_rows[start..].chunks(500) {
        let sb_msg = proto::encode_server(&ServerMsg::ScrollbackUpdate {
            rows: chunk.to_vec(),
            replay: true,
        });
        // Stop if the client isn't draining — don't climb toward the hard cap.
        if client.outbuf.len() > CLIENT_SUPPRESS_HIGH
            || client.outbuf.len().saturating_add(sb_msg.len()) > CLIENT_OUTBUF_CAP
        {
            crate::log::info(&format!(
                "client fd {}: stopping scrollback replay at {sent} rows (outbuf {})",
                client.fd,
                client.outbuf.len()
            ));
            break;
        }
        if !client_send(client, &sb_msg) {
            return false;
        }
        sent += chunk.len();
    }
    true
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
    stream: crate::ipc::ConnStream,
    boot: &Bootstrap,
) -> io::Result<(u16, u16, Vec<Session>, Vec<ClientConn>)> {
    let mut client = wrap_accepted_stream(stream)?;
    // Bound the handshake read: a client that connects and stays silent
    // must not stall server startup forever.
    let _ = client.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let is_tcp = client.is_tcp();

    let (client_rows, client_cols, auth_token) = match proto::decode_client(&mut client) {
        Ok(ClientMsg::Identify {
            rows,
            cols,
            auth_token,
            ..
        }) => (rows, cols, auth_token),
        Ok(ClientMsg::ListSessions) => {
            // Respond with empty session list (no sessions yet) and signal retry.
            let msg = proto::encode_server(&ServerMsg::SessionList {
                sessions: vec![],
                address: crate::server::server_address().to_string(),
            });
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

    if !tcp_auth_ok(is_tcp, &auth_token) {
        let _ = proto::send(
            &mut client,
            &proto::encode_server(&ServerMsg::Error {
                msg: "authentication failed".into(),
            }),
        );
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authentication failed",
        ));
    }

    // Reserve 1 row for the status bar.
    let grid_rows = client_rows.saturating_sub(1);
    let grid_cols = client_cols;

    // Create the first session. Prefer LRMUX_INIT_* from `new-server -- cmd`
    // / fresh `new-session` so we don't leave an empty shell session and
    // then add a second one for the command.
    let session_name = boot.session_name.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "session".to_string())
    });
    let mut session = make_bootstrap_session(session_name, grid_rows, grid_cols, boot);
    let window = &mut session.windows[0];

    // Send IdentifyAck with grid dimensions (not client dimensions).
    let ack = proto::encode_server(&ServerMsg::IdentifyAck {
        rows: grid_rows,
        cols: grid_cols,
        version: crate::version::VERSION.to_string(),
        address: crate::server::server_address().to_string(),
    });
    proto::send(&mut client, &ack)?;

    // Full GridSnapshot (same path as NewWindow / window switch) so the
    // client resets its outer scroll region and renderer — a bare
    // GridUpdate left the first pane looking "cursed" vs Ctrl-A c.
    let (cursor_row, cursor_col, cursor_visible) = window.pane.cursor();
    let snapshot = proto::encode_server(&ServerMsg::GridSnapshot {
        rows: window.pane.rows,
        cols: window.pane.cols,
        cells: window.pane.snapshot(),
        cursor_row,
        cursor_col,
        cursor_visible,
    });
    proto::send(&mut client, &snapshot)?;
    let _ = window.pane.take_dirty_rows(); // snapshot already has full state

    // Send status bar.
    let status = proto::encode_server(&ServerMsg::StatusBarUpdate {
        session: session.name.clone(),
        windows: vec![window.name.clone()],
        active: 0,
        session_count: 1,
        high_output: false,
    });
    proto::send(&mut client, &status)?;

    let mut conn = ClientConn::new(client, true);
    conn.session_idx = 0;
    conn.active_window = 0;

    Ok((grid_rows, grid_cols, vec![session], vec![conn]))
}

fn wrap_accepted_stream(stream: crate::ipc::ConnStream) -> io::Result<crate::ipc::ConnStream> {
    // Unix and WebSocket skip TCP TLS wrapping (WS is already a framed transport;
    // put a TLS-terminating proxy in front for WSS).
    if !matches!(stream, crate::ipc::ConnStream::Tcp(_)) {
        return Ok(stream);
    }
    ipc::maybe_wrap_accepted_tcp(
        stream,
        crate::server::network(),
        crate::server::tls_server_config(),
    )
}

fn tcp_auth_ok(is_tcp: bool, token: &str) -> bool {
    let required = crate::server::runtime_psk();
    if required.is_empty() || !is_tcp {
        return true;
    }
    token == required
}

/// Accept a new client, do the handshake, and add it to the clients list.
/// New clients default to session 0, window 0.
fn accept_new_client(
    listener: &crate::ipc::ConnListener,
    sessions: &[Session],
    grid_rows: u16,
    grid_cols: u16,
    clients: &mut Vec<ClientConn>,
) -> io::Result<()> {
    match listener.accept() {
        Ok(stream) => {
            let mut stream = match wrap_accepted_stream(stream) {
                Ok(s) => s,
                Err(e) => {
                    let line = format!("{e}");
                    crate::log::warn(&format!("TCP accept: {line}"));
                    eprintln!("lrmux: {line}");
                    return Ok(());
                }
            };
            stream.set_nonblocking(false)?;
            // Bound the handshake read: a client that connects and stays
            // silent must not freeze the whole event loop.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let is_tcp = stream.is_tcp();

            let (attach, is_control, auth_token) = match proto::decode_client(&mut stream) {
                Ok(ClientMsg::Identify {
                    attach: a,
                    auth_token,
                    ..
                }) => (a, false, auth_token),
                Ok(ClientMsg::IdentifyControl { auth_token, .. }) => (false, true, auth_token),
                Ok(ClientMsg::ListSessions) => {
                    // Lightweight query: respond with session list and close.
                    let names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
                    let msg = proto::encode_server(&ServerMsg::SessionList {
                        sessions: names,
                        address: crate::server::server_address().to_string(),
                    });
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
            };

            if !tcp_auth_ok(is_tcp, &auth_token) {
                let _ = proto::send(
                    &mut stream,
                    &proto::encode_server(&ServerMsg::Error {
                        msg: "authentication failed".into(),
                    }),
                );
                return Ok(());
            }

            // Send IdentifyAck with grid dimensions.
            let ack = proto::encode_server(&ServerMsg::IdentifyAck {
                rows: grid_rows,
                cols: grid_cols,
                version: crate::version::VERSION.to_string(),
                address: crate::server::server_address().to_string(),
            });
            if proto::send(&mut stream, &ack).is_err() {
                return Ok(());
            }

            // New client defaults to session 0, window 0.
            let session_idx = 0usize;
            let active = 0usize;
            // Only send snapshot + status bar to interactive clients (attach=true).
            // CLI commands (attach=false) only need the IdentifyAck.
            if attach {
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
                        high_output: false,
                    });
                    let _ = proto::send(&mut stream, &status);
                }
            }

            let mut conn = ClientConn::new(stream, attach);
            conn.session_idx = session_idx;
            conn.active_window = active;
            conn.is_control = is_control;
            crate::log::info(&format!(
                "client connected: session_idx={session_idx}, window={active}, total clients={}",
                clients.len() + 1
            ));
            // Non-blocking from here on: sends go through the client's
            // outbuf so a stalled client can't freeze the event loop.
            let _ = conn.stream.set_nonblocking(true);
            clients.push(conn);
            // Like real tmux -CC, emit the initial state right away —
            // silence after the DCS makes iTerm2 think tmux is hung.
            if is_control {
                send_control_initial_state(clients.last_mut().unwrap(), sessions);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Find (session_idx, window_idx) for a pane id.
fn find_pane_by_id(sessions: &[Session], pane_id: u32) -> Option<(usize, usize)> {
    for (si, s) in sessions.iter().enumerate() {
        for (wi, w) in s.windows.iter().enumerate() {
            if w.pane.id == pane_id {
                return Some((si, wi));
            }
        }
    }
    None
}

/// Apply an attached client's OSC 10/11 palette onto a pane.
fn seed_pane_palette(
    sessions: &mut [Session],
    session_idx: usize,
    window_idx: usize,
    pal: super::capture::TerminalPalette,
) {
    if session_idx >= sessions.len() || window_idx >= sessions[session_idx].windows.len() {
        return;
    }
    let pane = &mut sessions[session_idx].windows[window_idx].pane;
    if let Some(rgb) = pal.fg {
        pane.default_fg = Some(rgb);
    }
    if let Some(rgb) = pal.bg {
        pane.default_bg = Some(rgb);
    }
}

fn seed_client_focus_palette(sessions: &mut [Session], clients: &[ClientConn], client_idx: usize) {
    let Some(c) = clients.get(client_idx) else {
        return;
    };
    seed_pane_palette(sessions, c.session_idx, c.active_window, c.palette);
}

/// Palette for HTML capture: pane cache, then viewers of that window, then
/// any attached client, then an explicit override from the request.
fn resolve_capture_palette(
    sessions: &[Session],
    clients: &[ClientConn],
    session_idx: usize,
    window_idx: usize,
    term_fg: Option<(u8, u8, u8)>,
    term_bg: Option<(u8, u8, u8)>,
) -> super::capture::TerminalPalette {
    let mut pal = sessions
        .get(session_idx)
        .and_then(|s| s.windows.get(window_idx))
        .map(|w| w.pane.terminal_palette())
        .unwrap_or_default();
    for c in clients {
        if c.attach
            && !c.is_control
            && c.session_idx == session_idx
            && c.active_window == window_idx
        {
            pal = pal.merge(c.palette);
        }
    }
    for c in clients {
        if (c.attach || c.is_control) && (c.palette.fg.is_some() || c.palette.bg.is_some()) {
            pal = pal.merge(c.palette);
        }
    }
    pal.merge(super::capture::TerminalPalette {
        fg: term_fg,
        bg: term_bg,
    })
}

/// Ask an attached client to query its real TTY for OSC 10/11.
/// Prefer a viewer of this window; fall back to any attached client
/// (including control-mode) so we still answer when `active_window` is
/// briefly wrong or the only client is `-CC`. Leaving these unanswered
/// triggers vim E1568 and wrong `background` / colorscheme detection.
fn proxy_osc_color_query(
    clients: &mut [ClientConn],
    session_idx: usize,
    window_idx: usize,
    pane_id: u32,
    query: &crate::vt::OscColorQuery,
) {
    let msg = proto::encode_server(&ServerMsg::TermOscQuery {
        pane_id,
        code: query.code,
        bell_terminated: query.bell_terminated,
    });
    // Pass 1: interactive client viewing this window.
    for c in clients.iter_mut() {
        if c.attach
            && !c.is_control
            && c.session_idx == session_idx
            && c.active_window == window_idx
            && client_send(c, &msg)
        {
            return;
        }
    }
    // Pass 2: any interactive attached client (same outer palette for a
    // local tty client; better than silence).
    for c in clients.iter_mut() {
        if c.attach && !c.is_control && client_send(c, &msg) {
            return;
        }
    }
    // Pass 3: control-mode clients — may still have a usable /dev/tty or
    // stdout connected to iTerm.
    for c in clients.iter_mut() {
        if c.attach && c.is_control && client_send(c, &msg) {
            return;
        }
    }
    crate::log::info(&format!(
        "OSC {} query from pane %{pane_id}: no client to proxy to",
        query.code
    ));
}

/// Send dirty rows + cursor to clients viewing a specific window in a specific session.
fn send_grid_update_to_window_viewers(
    clients: &mut Vec<ClientConn>,
    session_idx: usize,
    window_idx: usize,
    pane: &mut crate::server::pane::Pane,
) -> io::Result<()> {
    // Take pending scrollback rows and send them first. Chunk them — a
    // burst that scrolls thousands of lines at once would otherwise build
    // a single frame larger than the client outbuf cap and get the client
    // disconnected.
    let scrolled = pane.take_pending_scrollback();
    for chunk in scrolled.chunks(500) {
        let sb_msg = proto::encode_server(&ServerMsg::ScrollbackUpdate {
            rows: chunk.to_vec(),
            replay: false,
        });
        let mut i = 0;
        while i < clients.len() {
            if clients[i].session_idx == session_idx
                && clients[i].active_window == window_idx
                && !clients[i].is_control
            {
                if clients[i].suppressed || clients[i].outbuf.len() > CLIENT_SUPPRESS_HIGH {
                    // Client is behind: skip incremental updates and resync
                    // with a snapshot once its outbuf drains.
                    clients[i].suppressed = true;
                } else if !client_send(&mut clients[i], &sb_msg) {
                    clients.remove(i);
                    continue;
                }
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
        if clients[i].session_idx == session_idx
            && clients[i].active_window == window_idx
            && !clients[i].is_control
        {
            if clients[i].suppressed || clients[i].outbuf.len() > CLIENT_SUPPRESS_HIGH {
                clients[i].suppressed = true;
                i += 1;
            } else if !client_send(&mut clients[i], &msg) {
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
        if !client_send(&mut clients[i], msg) {
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
            let attach = data.get(4).copied().unwrap_or(1) != 0;
            let auth_token = if data.len() >= 9 {
                let len = u32::from_le_bytes([data[5], data[6], data[7], data[8]]) as usize;
                if data.len() >= 9 + len {
                    String::from_utf8_lossy(&data[9..9 + len]).into_owned()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            ClientMsg::Identify {
                rows,
                cols,
                attach,
                auth_token,
            }
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
            // NewSession: name flag + optional name, then cwd flag + optional cwd,
            // then command flag + optional command.
            if data.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "NewSession needs at least 1 byte",
                ));
            }
            let has_name = data[0];
            let mut pos = 1;
            let name = if has_name != 0 {
                if data.len() < pos + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewSession name needs 4-byte length",
                    ));
                }
                let len =
                    u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                        as usize;
                pos += 4;
                if data.len() < pos + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewSession name truncated",
                    ));
                }
                let n = String::from_utf8_lossy(&data[pos..pos + len]).into_owned();
                pos += len;
                Some(n)
            } else {
                None
            };
            let cwd = if pos < data.len() && data[pos] != 0 {
                pos += 1;
                if data.len() < pos + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewSession cwd needs 4-byte length",
                    ));
                }
                let len =
                    u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                        as usize;
                pos += 4;
                if data.len() < pos + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewSession cwd truncated",
                    ));
                }
                let c = String::from_utf8_lossy(&data[pos..pos + len]).into_owned();
                pos += len;
                Some(c)
            } else {
                pos += 1; // skip the 0 flag
                None
            };
            let command = if pos < data.len() && data[pos] != 0 {
                pos += 1;
                if data.len() < pos + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewSession command needs 4-byte length",
                    ));
                }
                let len =
                    u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                        as usize;
                pos += 4;
                if data.len() < pos + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "NewSession command truncated",
                    ));
                }
                Some(String::from_utf8_lossy(&data[pos..pos + len]).into_owned())
            } else {
                None
            };
            ClientMsg::NewSession { name, cwd, command }
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
                let w = data[off];
                off += 1;
                Some(w)
            } else {
                off += 1;
                None
            };
            let (format, colors) = if data.len() >= off + 2 {
                let f = data[off];
                let c = data[off + 1] != 0;
                off += 2;
                (f, c)
            } else {
                (0, false)
            };
            let (term_fg, term_bg) = if data.get(off) == Some(&1) {
                off += 1;
                let fg = if data.get(off) == Some(&1) {
                    if data.len() < off + 4 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "CaptureWindow term_fg truncated",
                        ));
                    }
                    let rgb = (data[off + 1], data[off + 2], data[off + 3]);
                    off += 4;
                    Some(rgb)
                } else {
                    off += 1;
                    None
                };
                let bg = if data.get(off) == Some(&1) {
                    if data.len() < off + 4 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "CaptureWindow term_bg truncated",
                        ));
                    }
                    Some((data[off + 1], data[off + 2], data[off + 3]))
                } else {
                    None
                };
                (fg, bg)
            } else {
                (None, None)
            };
            ClientMsg::CaptureWindow {
                session,
                window,
                format,
                colors,
                term_fg,
                term_bg,
            }
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
        0x15 => ClientMsg::GetLog,
        0x16 => {
            // IdentifyControl: rows (u16) + cols (u16)
            if data.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "IdentifyControl needs 4 bytes",
                ));
            }
            let rows = u16::from_le_bytes([data[0], data[1]]);
            let cols = u16::from_le_bytes([data[2], data[3]]);
            let auth_token = if data.len() >= 8 {
                let len = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
                if data.len() >= 8 + len {
                    String::from_utf8_lossy(&data[8..8 + len]).into_owned()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            ClientMsg::IdentifyControl {
                rows,
                cols,
                auth_token,
            }
        }
        0x17 => {
            // ControlCommand: 4-byte length + string
            if data.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ControlCommand needs 4-byte length",
                ));
            }
            let clen = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if data.len() < 4 + clen {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ControlCommand data truncated",
                ));
            }
            let line = String::from_utf8_lossy(&data[4..4 + clen]).into_owned();
            ClientMsg::ControlCommand { line }
        }
        0x18 => ClientMsg::Refresh,
        0x19 => {
            // TermOscReply: pane_id u32 + len u32 + data
            if data.len() < 8 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "TermOscReply needs pane_id + length",
                ));
            }
            let pane_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            let dlen = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
            if data.len() < 8 + dlen {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "TermOscReply data truncated",
                ));
            }
            ClientMsg::TermOscReply {
                pane_id,
                data: data[8..8 + dlen].to_vec(),
            }
        }
        0x1a => {
            // TermPalette: optional fg + optional bg
            let mut off = 0;
            let fg = if data.get(off) == Some(&1) {
                if data.len() < off + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "TermPalette fg truncated",
                    ));
                }
                let rgb = (data[off + 1], data[off + 2], data[off + 3]);
                off += 4;
                Some(rgb)
            } else {
                off += 1;
                None
            };
            let bg = if data.get(off) == Some(&1) {
                if data.len() < off + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "TermPalette bg truncated",
                    ));
                }
                Some((data[off + 1], data[off + 2], data[off + 3]))
            } else {
                None
            };
            ClientMsg::TermPalette { fg, bg }
        }
        0x1b => {
            // SetPsk: 4-byte length + string
            if data.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SetPsk needs 4-byte length",
                ));
            }
            let plen = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if data.len() < 4 + plen {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SetPsk data truncated",
                ));
            }
            let psk = String::from_utf8_lossy(&data[4..4 + plen]).into_owned();
            ClientMsg::SetPsk { psk }
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

/// Accept a connection from any of the given listeners (blocking).
/// Used during the initial handshake to wait for the first client
/// from either a Unix or TCP listener.
fn accept_from_any(listeners: &[crate::ipc::ConnListener]) -> io::Result<crate::ipc::ConnStream> {
    if listeners.len() == 1 {
        return listeners[0].accept();
    }
    // Poll all listener fds and accept from the first ready one.
    let mut fds: Vec<libc::pollfd> = listeners
        .iter()
        .map(|l| libc::pollfd {
            fd: l.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ret = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }
        return Err(err);
    }
    for (i, fd) in fds.iter().enumerate() {
        if fd.revents & libc::POLLIN != 0 {
            return listeners[i].accept();
        }
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "no listener ready",
    ))
}

// ── Control mode support ────────────────────────────────────────────

/// Forward raw PTY output to all control clients viewing the given window.
/// The output is escaped and sent as %output notifications.
fn forward_output_to_control_clients(clients: &mut [ClientConn], pane_id: u32, raw: &[u8]) {
    let pane_id_str = format!("%{}", pane_id);
    // Chunk large bursts into multiple %output lines — a single escaped
    // notification for a multi-MB read could exceed the client outbuf cap.
    // Split on UTF-8 character boundaries so a multi-byte glyph is never
    // torn across notifications (defense in depth alongside pane buffering).
    let mut offset = 0;
    while offset < raw.len() {
        let mut end = (offset + 256 * 1024).min(raw.len());
        if end < raw.len() {
            let tail = super::pane::incomplete_utf8_tail_len(&raw[offset..end]);
            if tail > 0 && tail < end - offset {
                end -= tail;
            }
        }
        let chunk = &raw[offset..end];
        offset = end;
        let line = format!("%output {} {}", pane_id_str, escape_output(chunk));
        for client in clients.iter_mut() {
            // iTerm2 routes %output by pane id to the right tab, so send to
            // every control client — a window may live in a session other
            // than the client's attached session (affinity migration maps
            // each iTerm2 window to its own lrmux session).
            if !client.is_control {
                continue;
            }
            if client.suppressed {
                // Fast-forward mode: the client is behind, drop intermediate
                // output until it drains or sends a new command (any key).
                continue;
            }
            if client.outbuf.len() + line.len() > CLIENT_SUPPRESS_HIGH {
                client.suppressed = true;
                crate::log::info(&format!(
                    "control client fd {} entering fast-forward mode",
                    client.fd
                ));
                send_control_notify(
                    client,
                    "%message lrmux: high output detected — press any key to resume",
                );
                continue;
            }
            send_control_notify(client, &line);
        }
    }
}

/// Send a control notification line to a control client.
fn send_control_notify(client: &mut ClientConn, line: &str) {
    let msg = proto::encode_server(&ServerMsg::ControlNotify {
        line: line.to_string(),
    });
    let _ = client_send(client, &msg);
}

/// Send a control notification line to every control client.
fn broadcast_control_notify(clients: &mut [ClientConn], line: &str) {
    for c in clients.iter_mut() {
        if c.is_control {
            send_control_notify(c, line);
        }
    }
}

/// On server shutdown, send `%exit` to control clients and give every
/// client a short window to drain its outbuf before the socket closes.
/// Without the drain, a queued `%exit`/`PaneExit` is lost when the fd
/// closes — iTerm2 then keeps writing queued tmux commands (e.g.
/// `refresh-client -B ...`) into the shell the control session ran in.
fn shutdown_flush(clients: &mut [ClientConn]) {
    for c in clients.iter_mut() {
        if c.is_control {
            send_control_notify(c, "%exit");
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    loop {
        let mut pending = false;
        for c in clients.iter_mut() {
            if !c.outbuf.is_empty() {
                let _ = flush_client_outbuf(c);
                pending |= !c.outbuf.is_empty();
            }
        }
        if !pending || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Build a default @affinities value: one class per session containing all
/// of that session's window ids, in the `ids;` format iTerm2 writes.
/// Returned only when no real value was ever stored — once iTerm2 sends a
/// `set @affinities`, its value takes precedence.
fn synthesize_affinities(sessions: &[Session]) -> String {
    sessions
        .iter()
        .filter(|s| !s.windows.is_empty())
        .map(|s| {
            s.windows
                .iter()
                .map(|w| w.id.to_string())
                .collect::<Vec<_>>()
                .join(",")
                + ";"
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Decode an iTerm2-encoded user option value: `<prefix>` + hex of UTF-8.
/// e.g. `a_312c32` → "1,2". Returns the value unchanged if not encoded.
fn decode_iterm_encoded(value: &str, prefix: &str) -> String {
    let Some(hex) = value.strip_prefix(prefix) else {
        return value.to_string();
    };
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect();
    bytes
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| value.to_string())
}

/// Reconcile sessions with iTerm2 affinity classes.
///
/// iTerm2 groups tmux windows into "affinity classes" — one class per macOS
/// window (each member becomes a tab) — and persists them in the session
/// user option `@affinities` as `a_<hex>`. Decoded, the value is a
/// space-separated list of `id1,id2,...,GUID;opts` classes.
///
/// lrmux maps one iTerm2 window to one session, so a tmux window that lands
/// in a class of its own (⌘N, "new OS window") is moved into a new session;
/// conversely, a window dragged into another class joins that class's
/// session, and sessions left empty are removed.
///
/// Returns true if any window changed session (callers should emit
/// %sessions-changed).
fn migrate_affinity_sessions(
    sessions: &mut Vec<Session>,
    _target_si: usize,
    raw_value: &str,
    clients: &mut [ClientConn],
) -> bool {
    let raw = decode_iterm_encoded(raw_value, "a_");
    // Classes in written order; keep only numeric tokens (window ids —
    // GUIDs contain '-' and letters).
    let classes: Vec<Vec<u32>> = raw
        .split(' ')
        .map(|cls| {
            cls.split(';')
                .next()
                .unwrap_or("")
                .split(',')
                .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
                .filter_map(|t| t.parse::<u32>().ok())
                .collect()
        })
        .filter(|c: &Vec<u32>| !c.is_empty())
        .collect();
    if classes.is_empty() {
        return false;
    }

    // window id -> (session idx, window idx)
    let mut win_loc: HashMap<u32, (usize, usize)> = HashMap::new();
    for (si, s) in sessions.iter().enumerate() {
        for (wi, w) in s.windows.iter().enumerate() {
            win_loc.insert(w.id, (si, wi));
        }
    }

    // Assign each class to the session of its first member that doesn't
    // already own a class; if all member sessions own earlier classes, the
    // class gets a brand-new session. Then move any member windows whose
    // session isn't the class owner into it.
    let mut used: HashMap<usize, ()> = HashMap::new(); // session idx already owns a class
    let mut moves: Vec<(u32, usize, usize)> = Vec::new(); // (window id, from si, to si)
    for ids in &classes {
        let members: Vec<(u32, usize, usize)> = ids
            .iter()
            .filter_map(|id| win_loc.get(id).map(|&(si, wi)| (*id, si, wi)))
            .collect();
        if members.is_empty() {
            continue;
        }
        let owner_si = match members
            .iter()
            .map(|m| m.1)
            .find(|si| !used.contains_key(si))
        {
            Some(si) => si,
            None => {
                let (si0, wi0) = (members[0].1, members[0].2);
                let base = sessions[si0].windows[wi0].name.clone();
                let name = ensure_unique_session_name(&base, sessions);
                sessions.push(Session::new_empty(name));
                sessions.len() - 1
            }
        };
        used.insert(owner_si, ());
        for (id, si, _wi) in members {
            if si != owner_si {
                moves.push((id, si, owner_si));
            }
        }
    }
    if moves.is_empty() {
        return false;
    }

    crate::log::info(&format!(
        "affinities: moving windows {:?} between sessions",
        moves.iter().map(|m| m.0).collect::<Vec<_>>()
    ));

    // Apply moves: remove from source sessions (highest window idx first)
    // and push into destination sessions.
    let mut detached: Vec<(u32, usize, Window)> = Vec::new();
    for (id, from_si, to_si) in &moves {
        if let Some(pos) = sessions[*from_si].windows.iter().position(|w| w.id == *id) {
            let w = sessions[*from_si].windows.remove(pos);
            detached.push((*id, *to_si, w));
        }
    }
    for (_, to_si, w) in detached {
        sessions[to_si].windows.push(w);
    }

    // Remove sessions left without windows (fix up client session_idx).
    let empty: Vec<usize> = (0..sessions.len())
        .filter(|&si| sessions[si].windows.is_empty())
        .collect();
    for si in empty.into_iter().rev() {
        crate::log::info(&format!(
            "session '{}' emptied by affinity migration, removing",
            sessions[si].name
        ));
        sessions.remove(si);
        for c in clients.iter_mut() {
            if c.session_idx == si {
                c.session_idx = 0;
                c.active_window = 0;
            } else if c.session_idx > si {
                c.session_idx -= 1;
            }
        }
    }
    true
}

/// Escape binary data for %output notifications (tmux control mode format).
/// Characters < 0x20 and backslash are replaced with \nnn octal escapes.
/// Complete UTF-8 sequences are kept raw. Incomplete or invalid bytes are
/// also octal-escaped — never replaced with U+FFFD — so a multi-byte
/// character split across reads still reassembles correctly in iTerm2.
fn escape_output(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b < 0x20 || b == b'\\' {
            out.push_str(&format!("\\{:03o}", b));
            i += 1;
            continue;
        }
        if b < 0x80 {
            out.push(b as char);
            i += 1;
            continue;
        }
        let width = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        };
        if width > 0
            && i + width <= data.len()
            && let Ok(s) = std::str::from_utf8(&data[i..i + width])
        {
            out.push_str(s);
            i += width;
            continue;
        }
        // Invalid or incomplete — emit as octal so the raw byte survives.
        out.push_str(&format!("\\{:03o}", b));
        i += 1;
    }
    out
}

/// Current unix time in seconds (for %begin/%end blocks).
fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Send a %begin/%end response block with a real timestamp, an
/// incrementing command sequence number, and the given flags.
/// iTerm2's TmuxGateway requires flags&1 on responses to client-issued
/// commands: with flags=0 it treats the block as server-originated and
/// never pops its command queue, so callbacks (e.g. version detection)
/// never run and no windows open.
fn control_respond_flags(client: &mut ClientConn, lines: &[&str], flags: u8) {
    client.control_seq += 1;
    let ts = unix_ts();
    let seq = client.control_seq;
    send_control_notify(client, &format!("%begin {ts} {seq} {flags}"));
    for l in lines {
        send_control_notify(client, l);
    }
    send_control_notify(client, &format!("%end {ts} {seq} {flags}"));
}

/// Response to a client-issued command (flags=1).
fn control_respond(client: &mut ClientConn, lines: &[&str]) {
    control_respond_flags(client, lines, 1);
}

/// Send initial state notifications to a control client.
/// Mirrors what real `tmux -CC` emits right after the DCS: an empty
/// %begin/%end block, then %window-add for each window, then
/// %sessions-changed / %session-changed. Sending these immediately is
/// required — iTerm2 treats silence after the DCS as a hung tmux.
fn send_control_initial_state(client: &mut ClientConn, sessions: &[Session]) {
    // Empty %begin/%end block, like tmux emits first. Server-originated,
    // so flags=0 — iTerm2 uses this first block to kick off its command
    // queue and does not expect a queued command for it.
    control_respond_flags(client, &[], 0);

    if sessions.is_empty() {
        return;
    }

    let session = &sessions[client.session_idx];

    // %window-add for each window in the current session (tmux order).
    for window in &session.windows {
        send_control_notify(client, &format!("%window-add {}", window.id_str()));
    }

    send_control_notify(client, "%sessions-changed");
    send_control_notify(
        client,
        &format!("%session-changed {} {}", session.id_str(), session.name),
    );

    // Per-window metadata: name + layout.
    for window in &session.windows {
        send_control_notify(
            client,
            &format!("%window-renamed {} {}", window.id_str(), window.name),
        );
        // %layout-change @N <checksum>,<width>x<height>,<x>,<y>,<pane-id>
        // width=columns, height=rows (iTerm2's TmuxLayoutParser expects this).
        let layout = window_layout_str(window);
        send_control_notify(
            client,
            &format!("%layout-change {} {}", window.id_str(), layout),
        );
    }
}

/// Extract the -F format argument from a command line.
/// Handles `-F "..."`, `-F '...'`, and `-F ...` forms, anywhere in args
/// (iTerm2 sends e.g. `list-panes -t "%1" -F "..."`).
fn extract_format_arg(args: &str) -> Option<String> {
    // Find a whitespace-delimited token that is exactly "-F"; the format
    // is the next token (possibly quoted, possibly containing spaces).
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Find end of this token (respecting quotes).
        let start = i;
        let mut end = i;
        while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        let token = &args[start..end];
        if token == "-F" {
            // Next token is the format string.
            let mut j = end;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= bytes.len() {
                return None;
            }
            if bytes[j] == b'"' || bytes[j] == b'\'' {
                let q = bytes[j];
                if let Some(close) = args[j + 1..].find(q as char) {
                    return Some(args[j + 1..j + 1 + close].to_string());
                }
                return Some(args[j + 1..].to_string());
            }
            let mut k = j;
            while k < bytes.len() && !bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            return Some(args[j..k].to_string());
        }
        i = end;
    }
    None
}

/// Expand a tmux -F format string for a session/window.
/// Supports #{...} variables used by iTerm2 plus common ones, and \t / \n escapes.
fn expand_format(
    fmt: &str,
    session: Option<&Session>,
    window: Option<&Window>,
    socket_path: &std::path::Path,
) -> String {
    let mut out = String::with_capacity(fmt.len() * 2);
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else if c == '#' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var.push(c);
            }
            out.push_str(&format_var(&var, session, window, socket_path));
        } else {
            out.push(c);
        }
    }
    out
}

/// tmux layout string for a single-pane window:
/// <checksum>,<width>x<height>,<x>,<y>,<pane-id>
/// iTerm2's TmuxLayoutParser expects width=columns first, height=rows second.
fn window_layout_str(window: &Window) -> String {
    format!(
        "beef,{}x{},0,0,{}",
        window.pane.cols, window.pane.rows, window.pane.id
    )
}

/// Resolve a single #{...} format variable.
fn format_var(
    var: &str,
    session: Option<&Session>,
    window: Option<&Window>,
    socket_path: &std::path::Path,
) -> String {
    match var {
        "socket_path" => socket_path.to_string_lossy().into_owned(),
        "pid" => std::process::id().to_string(),
        "version" => "3.4".to_string(),
        "session_id" => session.map(|s| s.id_str()).unwrap_or_default(),
        "session_name" => session.map(|s| s.name.clone()).unwrap_or_default(),
        "session_windows" => session
            .map(|s| s.windows.len().to_string())
            .unwrap_or_default(),
        "window_id" => window.map(|w| w.id_str()).unwrap_or_default(),
        "window_name" => window.map(|w| w.name.clone()).unwrap_or_default(),
        "window_index" => session
            .and_then(|s| window.and_then(|w| s.windows.iter().position(|x| x.id == w.id)))
            .map(|i| i.to_string())
            .unwrap_or_default(),
        "window_width" | "window_height" => window
            .map(|w| {
                if var == "window_width" {
                    w.pane.cols.to_string()
                } else {
                    w.pane.rows.to_string()
                }
            })
            .unwrap_or_default(),
        "pane_id" => window
            .map(|w| format!("%{}", w.pane.id))
            .unwrap_or_default(),
        "pane_width" => window.map(|w| w.pane.cols.to_string()).unwrap_or_default(),
        "pane_height" => window.map(|w| w.pane.rows.to_string()).unwrap_or_default(),
        "pane_pid" => window
            .map(|w| w.pane.pty.child_pid.to_string())
            .unwrap_or_default(),
        "window_panes" => "1".to_string(),
        "pane_active" | "window_active" | "session_attached" => "1".to_string(),
        "window_layout" | "window_visible_layout" => {
            window.map(window_layout_str).unwrap_or_default()
        }
        "window_flags" => "*".to_string(),
        "pane_border_status" | "pane-border-status" => "off".to_string(),
        // Fields requested by iTerm2's TmuxStateParser via
        // `list-panes -F "key=#{key}..."`. Values we don't track are
        // reported as 0 — iTerm2 parses them as booleans/ints.
        "alternate_on"
        | "insert_flag"
        | "keypad_cursor_flag"
        | "keypad_flag"
        | "wrap_flag"
        | "mouse_standard_flag"
        | "mouse_button_flag"
        | "mouse_any_flag"
        | "mouse_utf8_flag"
        | "mouse_sgr_flag"
        | "bracket_paste_flag"
        | "pane_key_mode" => "0".to_string(),
        "cursor_flag" => window
            .map(|w| {
                if w.pane.grid.cursor_visible {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            })
            .unwrap_or_else(|| "1".to_string()),
        "cursor_x" => window
            .map(|w| w.pane.grid.cursor_col.to_string())
            .unwrap_or_else(|| "0".to_string()),
        "cursor_y" => window
            .map(|w| w.pane.grid.cursor_row.to_string())
            .unwrap_or_else(|| "0".to_string()),
        "alternate_saved_x" | "alternate_saved_y" => "0".to_string(),
        "scroll_region_upper" => "0".to_string(),
        "scroll_region_lower" => window
            .map(|w| (w.pane.rows - 1).to_string())
            .unwrap_or_default(),
        "pane_tabs" => window
            .map(|w| {
                // tmux reports comma-separated tab stops (every 8 cols).
                // A single value equal to pane width makes iTerm2 treat the
                // right margin as the only stop — tabs in live %output then
                // jump there (e.g. red git-status text appears as one char
                // on the right edge). capture-pane expands tabs in the grid,
                // which is why detach/reattach looked fine.
                let cols = w.pane.cols as usize;
                (8..cols)
                    .step_by(8)
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default(),
        _ => {
            // Conditional format: #{?condition,true-value,false-value}
            if let Some(rest) = var.strip_prefix('?') {
                let parts: Vec<&str> = rest.splitn(3, ',').collect();
                if parts.len() == 3 {
                    let truthy = !format_var(parts[0], session, window, socket_path).is_empty()
                        && format_var(parts[0], session, window, socket_path) != "0";
                    return if truthy {
                        parts[1].to_string()
                    } else {
                        parts[2].to_string()
                    };
                }
            }
            String::new()
        }
    }
}

/// Split a control-mode line into individual commands on unquoted `;`
/// separators (tmux command-list syntax). iTerm2 batches whole init
/// sequences this way (`sendCommandList` joins dicts with "; "), and each
/// sub-command must produce its own %begin/%end block — one per queued
/// commandDict.
fn split_command_list(line: &str) -> Vec<&str> {
    let mut cmds = Vec::new();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !in_single => i += 1, // skip escaped char
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b';' if !in_single && !in_double => {
                let part = line[start..i].trim();
                if !part.is_empty() {
                    cmds.push(part);
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let tail = line[start..].trim();
    if !tail.is_empty() {
        cmds.push(tail);
    }
    cmds
}

/// Handle a tmux-style command line from a control-mode client.
/// The line may be a `;`-separated command list; each sub-command gets its
/// own %begin/%end block. Returns true if the server should shut down.
///
/// Consecutive `send`/`send-keys` targeting the same pane are coalesced into
/// a single PTY `write_input` so iTerm2's split arrow-key lists
/// (`send -H 1b; send 0x5b; send -lt D`) arrive as one ESC [ D sequence,
/// while still emitting one %begin/%end per sub-command for the command queue.
fn handle_control_command(
    client: &mut ClientConn,
    sessions: &mut Vec<Session>,
    line: &str,
    socket_path: &std::path::Path,
    pending_affinities: &mut Option<(usize, String)>,
) -> bool {
    crate::log::log(crate::log::Level::Info, &format!("control cmd: {}", line));
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    let cmds = split_command_list(line);
    let mut i = 0;
    while i < cmds.len() {
        let cmd = cmds[i].trim();
        if cmd.is_empty() {
            i += 1;
            continue;
        }
        let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
        let cmd_name = crate::cmd::canonical_name(parts[0]);

        if cmd_name == "send-keys" {
            // Gather consecutive send-keys to the same target.
            let mut coalesced: Vec<u8> = Vec::new();
            let mut n_respond = 0;
            let mut group_loc: Option<(usize, usize)> = None;
            while i < cmds.len() {
                let c = cmds[i].trim();
                if c.is_empty() {
                    i += 1;
                    continue;
                }
                let p: Vec<&str> = c.splitn(2, ' ').collect();
                if crate::cmd::canonical_name(p[0]) != "send-keys" {
                    break;
                }
                let args_str = p.get(1).unwrap_or(&"");
                let (keys, target_str) = parse_send_keys_args(args_str);
                let loc = resolve_target(sessions, client, target_str.as_deref());
                if n_respond == 0 {
                    group_loc = loc;
                } else if loc != group_loc {
                    // Different pane — flush this group first.
                    break;
                }
                coalesced.extend_from_slice(&keys);
                n_respond += 1;
                i += 1;
            }
            if let Some((si, wi)) = group_loc {
                let window = &mut sessions[si].windows[wi];
                if !window.pane.exited && !coalesced.is_empty() {
                    let _ = window.pane.write_input(&coalesced);
                }
            }
            for _ in 0..n_respond {
                control_respond(client, &[]);
            }
            continue;
        }

        if handle_single_control_command(client, sessions, cmd, socket_path, pending_affinities) {
            return true;
        }
        i += 1;
    }
    false
}

fn handle_single_control_command(
    client: &mut ClientConn,
    sessions: &mut Vec<Session>,
    line: &str,
    socket_path: &std::path::Path,
    pending_affinities: &mut Option<(usize, String)>,
) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }

    // Split into command name and args.
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    let cmd_name = crate::cmd::canonical_name(parts[0]);
    let args_str = parts.get(1).unwrap_or(&"");

    // Helper: send a %begin/%end response with optional output lines.
    let respond = |client: &mut ClientConn, lines: &[&str]| control_respond(client, lines);

    match cmd_name {
        "refresh-client" => {
            // iTerm2 sends: refresh-client -fpause-after=0,wait-exit
            //               refresh-client -C <cols>,<rows>   (client size)
            //               refresh-client -C @N:<cols>x<rows> (per-window)
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            if let Some(sz) = parsed.get("C").or_else(|| parsed.get("size")) {
                // Parse "@N:WxH" or "W,H".
                let (win_target, dims) = match sz.split_once(':') {
                    Some((w, d)) => (Some(w.to_string()), d.to_string()),
                    None => (None, sz.to_string()),
                };
                let (cols, rows) = dims
                    .split_once('x')
                    .or_else(|| dims.split_once(','))
                    .map(|(a, b)| (a.parse::<u16>(), b.parse::<u16>()))
                    .map(|(a, b)| (a.ok(), b.ok()))
                    .unwrap_or((None, None));
                if let (Some(cols), Some(rows)) = (cols, rows) {
                    let loc = resolve_target(sessions, client, win_target.as_deref());
                    if let Some((si, wi)) = loc {
                        let window = &mut sessions[si].windows[wi];
                        if cols > 0
                            && rows > 0
                            && (window.pane.cols != cols || window.pane.rows != rows)
                        {
                            window.pane.resize(rows, cols);
                            send_control_notify(
                                client,
                                &format!(
                                    "%layout-change {} {}",
                                    window.id_str(),
                                    window_layout_str(window)
                                ),
                            );
                        }
                    }
                }
            }
            respond(client, &[]);
        }
        "show-option" | "show-options" => {
            // iTerm2 queries various options. Respond with empty/default values.
            // Examples: show-option -g -v status, show-option -q -g -v focus-events
            //           show-options -v -s default-terminal, show-options -g message-style
            //           show -v -q -t $N @affinities (session user options)
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let t = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|s| s.to_string());
            // Last positional starting with '@' = session user option.
            let user_opt = parsed
                .positional
                .iter()
                .find(|a| a.starts_with('@'))
                .map(|s| s.to_string());
            if let Some(key) = user_opt {
                let si = resolve_session_target(sessions, client, t.as_deref());
                let stored = si
                    .and_then(|i| sessions.get(i))
                    .and_then(|s| s.options.get(&key))
                    .cloned();
                // Without saved affinities iTerm2 opens every window in its
                // own OS window. Synthesize one class per session so each
                // session's windows open as tabs of a single OS window —
                // matching the lrmux session↔window model.
                let val = stored.unwrap_or_else(|| {
                    if key == "@affinities" {
                        synthesize_affinities(sessions)
                    } else {
                        String::new()
                    }
                });
                if val.is_empty() {
                    respond(client, &[]);
                } else {
                    respond(client, &[val.as_str()]);
                }
            } else if args_str.contains("default-terminal") {
                respond(client, &["screen-256color"]);
            } else if args_str.contains("focus-events") {
                respond(client, &["off"]);
            } else if args_str.contains("status") {
                respond(client, &["on"]);
            } else {
                respond(client, &[]);
            }
        }
        "set-option" => {
            // iTerm2 sends: set -t $N @affinities "...", set -t $N @hidden ...
            // Store session user options so they survive reattach.
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let t = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|s| s.to_string());
            if parsed.positional.len() >= 2 && parsed.positional[0].starts_with('@') {
                let key = parsed.positional[0].clone();
                let val = unquote(&parsed.positional[1]).to_string();
                if let Some(si) = resolve_session_target(sessions, client, t.as_deref())
                    && let Some(s) = sessions.get_mut(si)
                {
                    s.options.insert(key.clone(), val.clone());
                    if key == "@affinities" {
                        // iTerm2 assigns each OS window an affinity class of
                        // window ids; windows in a new class belong to a new
                        // session (handled by the caller, which owns `clients`).
                        *pending_affinities = Some((si, val));
                    }
                }
            }
            respond(client, &[]);
        }
        "show-window-options" => {
            // iTerm2 queries: show-window-options -g aggressive-resize
            //                  show-window-options pane-border-format
            respond(client, &[]);
        }
        "list-sessions" => {
            // iTerm2 sends: list-sessions -F "<format>"
            let lines: Vec<String> = if let Some(fmt) = extract_format_arg(args_str) {
                sessions
                    .iter()
                    .map(|s| expand_format(&fmt, Some(s), None, socket_path))
                    .collect()
            } else {
                sessions
                    .iter()
                    .map(|s| format!("{}: {} (1 windows) [{}x{}]", s.id_str(), s.name, 24, 80))
                    .collect()
            };
            let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
            respond(client, &refs);
        }
        "list-windows" => {
            // iTerm2 sends: list-windows -F "#{socket_path}", -F "#{pid}", etc.
            //               list-windows -F "<window TSV>" -t "$N" (per-session)
            // Without -t, return windows of ALL sessions: lrmux maps each
            // iTerm2 window to its own session, and iTerm2 groups the listed
            // windows into OS windows via their saved affinities.
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let t = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|s| s.to_string());
            let targets: Vec<&Session> = match t.as_deref() {
                Some(t) => resolve_session_target(sessions, client, Some(t))
                    .and_then(|si| sessions.get(si))
                    .into_iter()
                    .collect(),
                None => sessions.iter().collect(),
            };
            let mut lines: Vec<String> = Vec::new();
            if let Some(fmt) = extract_format_arg(args_str) {
                for session in &targets {
                    for w in &session.windows {
                        lines.push(expand_format(&fmt, Some(session), Some(w), socket_path));
                    }
                }
            } else {
                for session in &targets {
                    for (i, w) in session.windows.iter().enumerate() {
                        lines.push(format!(
                            "{}: {} [{}x{}] (0 panes) {}",
                            i,
                            w.name,
                            24,
                            80,
                            w.id_str()
                        ));
                    }
                }
            }
            let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
            respond(client, &refs);
        }
        "list-panes" => {
            // iTerm2 sends: list-panes -t "%N" -F "key=#{key}..." (per-pane
            // state dump when opening a window — cursor pos, modes, etc.)
            //               list-panes -s -t $N -F "#{pane_id}" (all panes
            //               in session $N)
            // The -t target MUST resolve to the requested pane — iTerm2
            // filters the state dump by pane_id and discards mismatches.
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let t = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|s| s.to_string());
            let session_wide = parsed.get("s").is_some() || args.iter().any(|a| a == "-s");

            if session_wide {
                // -s: list all panes in the target session (or client's).
                let si = resolve_session_target(sessions, client, t.as_deref());
                let lines: Vec<String> = match si.and_then(|i| sessions.get(i)) {
                    Some(s) => {
                        if let Some(fmt) = extract_format_arg(args_str) {
                            s.windows
                                .iter()
                                .map(|w| expand_format(&fmt, Some(s), Some(w), socket_path))
                                .collect()
                        } else {
                            s.windows
                                .iter()
                                .enumerate()
                                .map(|(i, w)| {
                                    format!(
                                        "{}: [{}x{}] [history 0/0] %{}",
                                        i, w.pane.cols, w.pane.rows, w.pane.id
                                    )
                                })
                                .collect()
                        }
                    }
                    None => Vec::new(),
                };
                let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
                respond(client, &refs);
            } else {
                let (session, window) = match resolve_target(sessions, client, t.as_deref()) {
                    Some((si, wi)) => (
                        sessions.get(si),
                        sessions.get(si).and_then(|s| s.windows.get(wi)),
                    ),
                    None => (None, None),
                };
                let lines: Vec<String> =
                    if let (Some(fmt), Some(w)) = (extract_format_arg(args_str), window) {
                        vec![expand_format(&fmt, session, Some(w), socket_path)]
                    } else if let Some(w) = window {
                        vec![format!(
                            "0: [{}x{}] [history 0/0] %{} (active)",
                            w.pane.cols, w.pane.rows, w.pane.id
                        )]
                    } else {
                        Vec::new()
                    };
                let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
                respond(client, &refs);
            }
        }
        "capture-pane" => {
            // iTerm2 sends: capture-pane -peqJN -t "%N" -S -<n> (history)
            //               capture-pane -p -P -C -t "%N" (pending output)
            // -e requests SGR escapes (colors/attrs), -S -<n> includes scrollback.
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            // -P asks only for output received since the last %output send.
            // We forward all pane output as %output immediately, so there is
            // never pending output — returning the grid here would make
            // iTerm2 append the whole screen as duplicate B&W history.
            if args.iter().any(|a| {
                a.len() >= 2 && a.starts_with('-') && !a.starts_with("--") && a[1..].contains('P')
            }) {
                respond(client, &[]);
                return false;
            }
            let parsed = crate::cmd::parse_flags(&args);
            let t = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|s| s.to_string());
            let include_scrollback = args.iter().enumerate().any(|(i, a)| {
                let v = if a == "-S" {
                    args.get(i + 1).map(|s| s.as_str())
                } else {
                    a.strip_prefix("-S")
                };
                v.map(|v| v == "-" || v.parse::<i64>().map(|n| n < 0).unwrap_or(false))
                    .unwrap_or(false)
            });
            // tmux `-e` = include SGR; `-c` / `--colors` = include styles in
            // the chosen --format. Without colors, formats emit text only.
            let tmux_e = args.iter().any(|a| {
                a.len() >= 2
                    && a.starts_with('-')
                    && !a.starts_with("--")
                    && a[1..]
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_alphabetic())
                        .unwrap_or(false)
                    && a[1..].contains('e')
            });
            let colors = tmux_e || parsed.has("colors") || parsed.flags.contains_key("c");
            let format = parsed
                .get("format")
                .and_then(|s| super::capture::CaptureFormat::parse(s).ok())
                .unwrap_or(if tmux_e {
                    super::capture::CaptureFormat::Ansi
                } else {
                    super::capture::CaptureFormat::Ascii
                });
            let window = resolve_target(sessions, client, t.as_deref())
                .and_then(|(si, wi)| sessions.get(si).and_then(|s| s.windows.get(wi)));
            let text = window
                .map(|w| {
                    super::capture::render_pane(
                        &w.pane,
                        format,
                        colors,
                        include_scrollback,
                        super::capture::TerminalPalette::default(),
                    )
                })
                .unwrap_or_default();
            let lines: Vec<&str> = if text.is_empty() {
                Vec::new()
            } else {
                text.lines().collect()
            };
            respond(client, &lines);
        }
        "display-message" => {
            // iTerm2 sends: display-message -p "#{version}", "#{pid}", and
            //   display -p -F "<window TSV>" -t @N  (window opener query)
            // Resolve -t so the format expands for the requested window.
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let t = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|s| s.to_string());
            let (session, window) = match resolve_target(sessions, client, t.as_deref()) {
                Some((si, wi)) => (
                    sessions.get(si),
                    sessions.get(si).and_then(|s| s.windows.get(wi)),
                ),
                None => (
                    sessions.get(client.session_idx),
                    sessions
                        .get(client.session_idx)
                        .and_then(|s| s.windows.get(client.active_window)),
                ),
            };
            if let Some(fmt) = extract_format_arg(args_str) {
                let text = expand_format(&fmt, session, window, socket_path);
                respond(client, &[&text]);
            } else if args_str.contains("#{") {
                // -p "<format>" without -F
                if let Some(start) = args_str.find('"') {
                    if let Some(end) = args_str.rfind('"') {
                        if end > start {
                            let fmt = &args_str[start + 1..end];
                            let session = sessions.get(client.session_idx);
                            let window = session.and_then(|s| s.windows.get(client.active_window));
                            let text = expand_format(fmt, session, window, socket_path);
                            respond(client, &[&text]);
                        } else {
                            respond(client, &[]);
                        }
                    } else {
                        respond(client, &[]);
                    }
                } else {
                    respond(client, &[]);
                }
            } else {
                respond(client, &[]);
            }
        }
        "list-keys" => {
            // iTerm2 sends: list-keys (to get key bindings)
            respond(client, &[]);
        }
        "copy-mode" => {
            // iTerm2 sends: copy-mode -q
            respond(client, &[]);
        }
        "new-session" => {
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let name = parsed
                .get("s")
                .or_else(|| parsed.get("session"))
                .map(|s| s.to_string());
            let cwd = parsed
                .get("c")
                .or_else(|| parsed.get("cwd"))
                .map(|s| s.to_string());

            // Create the session. Default name: basename of -c if given
            // (matches lrmux's cwd-derived session naming), else "session".
            let session_name = name
                .map(|n| unquote(&n).to_string())
                .or_else(|| {
                    cwd.as_deref().and_then(|c| {
                        std::path::Path::new(unquote(c))
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                    })
                })
                .unwrap_or_else(|| "session".to_string());
            let session_name = ensure_unique_session_name(&session_name, sessions);

            let new_session = if let Some(ref c) = cwd {
                Session::new_in_cwd(session_name, 24, 80, unquote(c))
            } else {
                Session::new(session_name, 24, 80)
            };
            let sid = new_session.id_str();
            let sname = new_session.name.clone();
            sessions.push(new_session);
            let si = sessions.len() - 1;
            client.session_idx = si;

            send_control_notify(client, "%sessions-changed");
            send_control_notify(client, &format!("%session-changed {} {}", sid, sname));
            let win = &sessions[si].windows[0];
            send_control_notify(client, &format!("%window-add {}", win.id_str()));
            send_control_notify(
                client,
                &format!("%window-renamed {} {}", win.id_str(), win.name),
            );

            respond(client, &[]);
        }
        "send-keys" => {
            let (keys, target_str) = parse_send_keys_args(args_str);
            let loc = resolve_target(sessions, client, target_str.as_deref());
            let Some((si, wi)) = loc else {
                respond(client, &[]);
                return false;
            };
            let window = &mut sessions[si].windows[wi];
            if !window.pane.exited {
                let _ = window.pane.write_input(&keys);
            }
            respond(client, &[]);
        }
        "kill-server" => {
            respond(client, &[]);
            send_control_notify(client, "%exit");
            // Signal the event loop to shut down gracefully (SIGHUP
            // children, save state, remove the socket) instead of
            // exit(0), which would orphan shells and skip cleanup.
            return true;
        }
        "new-window" => {
            // iTerm2 sends: new-window -PF '#{window_id}' -c '#{pane_current_path}'
            // -P means "print": the response must contain the new window id
            // (@N), which iTerm2 registers in _pendingWindows to open the tab.
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let print = parsed.get("P").is_some()
                || args.iter().any(|a| a.contains('P') && a.starts_with('-'));

            // Create a new window in the -t session ("$N" or "$N:+"), else
            // the client's session.
            let si = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|t| {
                    let t = unquote(t);
                    let t = t.split(':').next().unwrap_or(t);
                    t.to_string()
                })
                .and_then(|t| resolve_session_target(sessions, client, Some(&t)))
                .unwrap_or(client.session_idx);
            let mut wid_str = String::new();
            if si < sessions.len() {
                let session = &mut sessions[si];
                let (rows, cols) = session
                    .windows
                    .first()
                    .map(|w| (w.pane.rows, w.pane.cols))
                    .unwrap_or((24, 80));
                let win = Window::new(rows, cols, "shell".to_string());
                wid_str = win.id_str();
                let wname = win.name.clone();
                let layout = window_layout_str(&win);
                session.windows.push(win);
                send_control_notify(client, &format!("%window-add {}", wid_str));
                send_control_notify(client, &format!("%window-renamed {} {}", wid_str, wname));
                send_control_notify(client, &format!("%layout-change {} {}", wid_str, layout));
            }
            if print && !wid_str.is_empty() {
                respond(client, &[wid_str.as_str()]);
            } else {
                respond(client, &[]);
            }
        }
        "kill-window" | "kill-pane" => {
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let t = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|s| s.to_string());
            if let Some((si, wi)) = resolve_target(sessions, client, t.as_deref())
                && si < sessions.len()
                && wi < sessions[si].windows.len()
            {
                // SIGHUP the child, then remove the window.
                let window = &mut sessions[si].windows[wi];
                if !window.pane.exited {
                    unsafe {
                        libc::kill(window.pane.pty.child_pid.into(), libc::SIGHUP);
                    }
                }
                let wid = window.id_str();
                sessions[si].windows.remove(wi);
                send_control_notify(client, &format!("%window-close {}", wid));
                // Fix up this client's active_window if needed.
                if client.session_idx == si && client.active_window >= sessions[si].windows.len() {
                    client.active_window = sessions[si].windows.len().saturating_sub(1);
                }
            }
            respond(client, &[]);
        }
        "select-window" | "select-pane" => {
            // Track the client's active window so send-keys, list-panes,
            // capture-pane etc. target the pane iTerm2 is showing.
            let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
            let parsed = crate::cmd::parse_flags(&args);
            let t = parsed
                .get("t")
                .or_else(|| parsed.get("target"))
                .map(|s| s.to_string());
            if let Some((si, wi)) = resolve_target(sessions, client, t.as_deref()) {
                client.session_idx = si;
                client.active_window = wi;
            }
            respond(client, &[]);
        }
        "rename-window" => {
            // For now, just acknowledge.
            respond(client, &[]);
        }
        "detach-client" => {
            // Real tmux -CC on detach: %begin/%end (flags=1), then %exit,
            // then the client process exits. %client-detached is only sent
            // to *other* control clients.
            respond(client, &[]);
            send_control_notify(client, "%exit");
            client.close_when_idle = true;
        }
        _ => {
            // Unknown command — respond with empty %begin/%end (not error)
            // so iTerm2 doesn't give up and exit tmux mode.
            respond(client, &[]);
        }
    }
    false
}

/// Parse send-keys flags: -H (hex args), -l (literal), -t <target>.
/// Combined forms like "-lt" / "-H" come from iTerm2.
/// Returns (key bytes, optional target string).
fn parse_send_keys_args(args_str: &str) -> (Vec<u8>, Option<String>) {
    let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();
    let mut hex_mode = false;
    let mut target_str: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with('-') && a.len() > 1 {
            let flags = &a[1..];
            if flags == "t" || flags.ends_with('t') && flags.len() > 1 {
                // -t or -Xt: takes next arg as target.
                if i + 1 < args.len() {
                    target_str = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            if flags.contains('H') {
                hex_mode = true;
            }
        } else {
            positionals.push(a.clone());
        }
        i += 1;
    }
    let keys = if hex_mode {
        // -H: each positional is hex-encoded bytes ("0d", "1b", "0d0a").
        let mut out = Vec::new();
        for h in &positionals {
            let h = h.trim();
            let mut j = 0;
            while j + 2 <= h.len() {
                if let Ok(b) = u8::from_str_radix(&h[j..j + 2], 16) {
                    out.push(b);
                }
                j += 2;
            }
        }
        out
    } else {
        parse_tmux_keys_for_control(&positionals)
    };
    (keys, target_str)
}

/// Parse tmux-style key names into byte sequences (for control mode send-keys).
/// Reuses the same logic as main.rs::parse_tmux_keys.
fn parse_tmux_keys_for_control(parts: &[String]) -> Vec<u8> {
    let mut result = Vec::new();
    for part in parts {
        match part.as_str() {
            "Enter" | "Return" => result.push(b'\r'),
            "Tab" => result.push(b'\t'),
            "Escape" | "Esc" => result.push(0x1b),
            "Space" => result.push(b' '),
            "BS" | "BSpace" => result.push(0x7f),
            "Up" => result.extend_from_slice(b"\x1b[A"),
            "Down" => result.extend_from_slice(b"\x1b[B"),
            "Right" => result.extend_from_slice(b"\x1b[C"),
            "Left" => result.extend_from_slice(b"\x1b[D"),
            "Home" => result.extend_from_slice(b"\x1b[H"),
            "End" => result.extend_from_slice(b"\x1b[F"),
            "PageUp" | "PgUp" => result.extend_from_slice(b"\x1b[5~"),
            "PageDown" | "PgDn" => result.extend_from_slice(b"\x1b[6~"),
            "F1" => result.extend_from_slice(b"\x1bOP"),
            "F2" => result.extend_from_slice(b"\x1bOQ"),
            "F3" => result.extend_from_slice(b"\x1bOR"),
            "F4" => result.extend_from_slice(b"\x1bOS"),
            "F5" => result.extend_from_slice(b"\x1b[15~"),
            "F6" => result.extend_from_slice(b"\x1b[17~"),
            "F7" => result.extend_from_slice(b"\x1b[18~"),
            "F8" => result.extend_from_slice(b"\x1b[19~"),
            "F9" => result.extend_from_slice(b"\x1b[20~"),
            "F10" => result.extend_from_slice(b"\x1b[21~"),
            "F11" => result.extend_from_slice(b"\x1b[23~"),
            "F12" => result.extend_from_slice(b"\x1b[24~"),
            _ if part.starts_with("C-") && part.len() == 3 => {
                let key = part.as_bytes()[2];
                if key.is_ascii_uppercase() {
                    result.push(key - b'A' + 1);
                } else if key.is_ascii_lowercase() {
                    result.push(key - b'a' + 1);
                } else if key == b'@' {
                    result.push(0x00);
                } else if key == b'[' {
                    result.push(0x1b);
                } else if key == b'\\' {
                    result.push(0x1c);
                } else if key == b']' {
                    result.push(0x1d);
                } else if key == b'^' {
                    result.push(0x1e);
                } else if key == b'_' {
                    result.push(0x1f);
                } else if key == b'?' {
                    result.push(0x7f);
                } else {
                    result.extend_from_slice(part.as_bytes());
                }
            }
            _ if part.starts_with("M-") && part.len() == 3 => {
                result.push(0x1b);
                result.push(part.as_bytes()[2]);
            }
            // Hex key code without -H: tmux treats 0xNN as a Unicode
            // codepoint and UTF-8-encodes it. iTerm2 sends non-ASCII keys
            // this way (e.g. ñ → `send -t %0 0xf1`). With -H (handled
            // above) the same form means raw bytes instead.
            _ if part.starts_with("0x") || part.starts_with("0X") => {
                if let Ok(cp) = u32::from_str_radix(&part[2..], 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut buf = [0u8; 4];
                        result.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                } else {
                    result.extend_from_slice(part.as_bytes());
                }
            }
            _ => {
                result.extend_from_slice(part.as_bytes());
            }
        }
    }
    result
}

/// Find a session by name (or return the first if name is None).
fn find_session(sessions: &[Session], name: &Option<String>) -> Option<usize> {
    match name {
        Some(n) => sessions
            .iter()
            .position(|s| s.name == *n || s.id_str() == *n),
        None => Some(0),
    }
}

/// Find a window by index (or return the first if index is None).
fn find_window(session: &Session, index: &Option<String>) -> Option<usize> {
    match index {
        Some(i) => {
            if let Some(id) = i.strip_prefix('@') {
                // Window id @N.
                session
                    .windows
                    .iter()
                    .position(|w| w.id_str() == *i || w.id.to_string() == id)
                    .or_else(|| i.parse::<usize>().ok())
            } else {
                i.parse::<usize>().ok()
            }
        }
        None => Some(0),
    }
}

/// Strip one layer of surrounding quotes — iTerm2 sends -t "%1" / -t @5
/// with literal quotes that must not confuse target parsing.
fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Resolve a tmux -t target to (session_idx, window_idx).
/// "%N" = pane id (searched across sessions), "@N" = window id,
/// "$N" = session id, "name"/"name:idx"/"idx" = session/window.
/// None = the client's current session and active window.
fn resolve_target(
    sessions: &[Session],
    client: &ClientConn,
    target: Option<&str>,
) -> Option<(usize, usize)> {
    let t = unquote(target.unwrap_or(""));
    if t.is_empty() {
        let si = client.session_idx;
        let wi = client.active_window.min(
            sessions
                .get(si)
                .map(|s| s.windows.len().saturating_sub(1))
                .unwrap_or(0),
        );
        return Some((si, wi));
    }
    if let Some(pid) = t.strip_prefix('%') {
        // Pane id: find the window containing it.
        let pid = pid.parse::<u32>().ok()?;
        for (si, s) in sessions.iter().enumerate() {
            for (wi, w) in s.windows.iter().enumerate() {
                if w.pane.id == pid {
                    return Some((si, wi));
                }
            }
        }
        return None;
    }
    if t.starts_with('@') {
        for (si, s) in sessions.iter().enumerate() {
            if let Some(wi) = find_window(s, &Some(t.to_string())) {
                return Some((si, wi));
            }
        }
        return None;
    }
    if t.starts_with('$') {
        // Session id: its first window.
        let si = resolve_session_target(sessions, client, Some(t))?;
        if sessions[si].windows.is_empty() {
            return None;
        }
        return Some((si, 0));
    }
    let parsed = crate::cmd::Target::parse(t);
    let si = find_session(sessions, &parsed.session).or(Some(client.session_idx))?;
    let wi = find_window(&sessions[si], &parsed.window)?;
    Some((si, wi))
}

#[cfg(test)]
mod escape_output_tests {
    use super::escape_output;

    #[test]
    fn preserves_box_drawing() {
        let dash = "─".as_bytes(); // E2 94 80
        assert_eq!(escape_output(dash), "─");
        let line = "────────────────────".as_bytes();
        assert_eq!(escape_output(line), "────────────────────");
    }

    #[test]
    fn never_emits_replacement_char() {
        // Incomplete leading byte of ─ — must be octal, not U+FFFD.
        let out = escape_output(&[0xE2]);
        assert!(!out.contains('\u{FFFD}'), "got {out:?}");
        assert_eq!(out, "\\342");

        let out = escape_output(&[0xE2, 0x94]);
        assert!(!out.contains('\u{FFFD}'), "got {out:?}");
        assert_eq!(out, "\\342\\224");
    }

    #[test]
    fn split_box_drawing_reassembles_via_octal() {
        // Simulate two %output payloads after a bad split; concatenating the
        // decoded escapes yields the original UTF-8 for ─.
        let a = escape_output(&[0xE2]);
        let b = escape_output(&[0x94, 0x80]);
        let mut bytes = Vec::new();
        for part in [a.as_str(), b.as_str()] {
            let mut chars = part.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    let o1 = chars.next().unwrap().to_digit(8).unwrap();
                    let o2 = chars.next().unwrap().to_digit(8).unwrap();
                    let o3 = chars.next().unwrap().to_digit(8).unwrap();
                    bytes.push(((o1 << 6) | (o2 << 3) | o3) as u8);
                } else {
                    let mut buf = [0u8; 4];
                    bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "─");
    }

    #[test]
    fn escapes_controls_and_backslash() {
        assert_eq!(escape_output(b"a\nb\\c"), "a\\012b\\134c");
    }
}

/// Resolve a session target: "$N" = session id, "name" = session name,
/// None = the client's current session.
fn resolve_session_target(
    sessions: &[Session],
    client: &ClientConn,
    target: Option<&str>,
) -> Option<usize> {
    match unquote(target.unwrap_or("")) {
        "" => Some(client.session_idx),
        t if t.starts_with('$') => {
            let id = t.strip_prefix('$')?.parse::<u32>().ok()?;
            sessions.iter().position(|s| s.id == id)
        }
        t => find_session(sessions, &Some(t.to_string())),
    }
}
