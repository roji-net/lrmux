// Client process: raw mode, input relay, prefix detection, output render.

pub mod control;
pub mod copy_mode;
mod input_filter;
pub mod inventory;
mod mouse;
pub mod render;
pub mod selector;
pub mod terminal;
pub mod tmux_shim;

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::grid::{Cell, Grid};
use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};
use crate::version;

/// Flash message produced by network/PSK setup (shown after confirm closes).
static NETWORK_SETUP_FLASH: Mutex<Option<String>> = Mutex::new(None);

/// Prefix key: Ctrl-A (0x01).
const PREFIX: u8 = 0x01;

/// Global flag set by SIGWINCH handler.
static WINCH: AtomicBool = AtomicBool::new(false);

/// SIGWINCH signal handler — just sets a flag.
extern "C" fn handle_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}

/// Install a SIGWINCH handler.
fn install_winch_handler() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_winch as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }
}

/// State of the prefix-key state machine.
enum PrefixState {
    /// Normal mode: all bytes pass through except the prefix.
    Normal,
    /// Prefix was pressed; waiting for the command byte.
    Command,
}

/// State of the confirmation dialog (for destructive actions).
enum ConfirmState {
    /// No confirmation active.
    None,
    /// "Kill current window? (y/n)" — waiting for a single keypress.
    KillWindow,
    /// "Type session name to confirm kill:" — waiting for text input + Enter.
    KillSession {
        input: String,
        target: String,
        window_count: usize,
    },
    /// "Rename session to:" — editable name prompt.
    RenameSession { input: String },
    /// Network setup scaffolding: choose generate / set PSK.
    NetworkSetupMenu,
    /// Type a PSK then Enter to apply.
    NetworkSetupSetPsk { input: String },
}

/// How an interactive client run ended.
pub enum ClientExit {
    /// Normal exit: explicit detach, stdin gone, error path.
    Done,
    /// The attached session/server went away under the client — the pane
    /// exited, the server shut down, or the socket dropped. Carries where
    /// they were attached so a selector-driven flow can reopen the picker
    /// (an explicit `lrmux attach` still exits to the prompt).
    SessionEnded(SelectHint),
    /// The user asked to leave this server and return to the session
    /// selector (`Ctrl-A /`). Carries where they were attached so the
    /// selector can pre-select that row.
    Selector(SelectHint),
}

/// Where the client was attached when it exited to the selector.
pub struct SelectHint {
    /// Server name (Unix socket) — `None` when attached over TCP.
    pub server: Option<String>,
    /// TCP address the client was attached to — `None` for Unix sockets.
    pub tcp: Option<String>,
    /// Session the client was viewing, if known.
    pub session: Option<String>,
}

/// Terminal-identity vars sent in Identify on interactive attach.
/// The server refreshes its spawn env with them (tmux
/// `update-environment`-style), so panes created later inherit the
/// freshest attaching terminal's capabilities instead of whatever env
/// the server happened to start with. `TERM` is deliberately absent:
/// the pane's TERM describes lrmux's own emulation, not the outer
/// terminal's terminfo name.
pub fn terminal_env_overlay() -> Vec<(String, String)> {
    const KEYS: &[&str] = &[
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "LC_TERMINAL",
        "TERMINAL_EMULATOR",
        "WEZTERM_EXECUTABLE",
        "KITTY_WINDOW_ID",
    ];
    KEYS.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Capture a pane's contents from a server. `tcp` overrides the local
/// Unix socket lookup of `server`. `window: None` captures the client's
/// active window (index 0 for this transient connection).
pub fn capture_pane(
    server: &str,
    tcp: Option<&str>,
    session: Option<String>,
    window: Option<u8>,
    format: u8,
    colors: bool,
) -> io::Result<String> {
    let mut stream = match tcp {
        Some(addr) => ipc::connect_tcp(addr)?,
        None => {
            let sock = ipc::socket_path(server);
            if !ipc::server_exists(&sock) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("server '{server}' is not running"),
                ));
            }
            ipc::ConnStream::Unix(ipc::connect(&sock)?)
        }
    };
    proto::send(
        &mut stream,
        &proto::encode_client(&ClientMsg::Identify {
            rows: 24,
            cols: 80,
            attach: false,
            auth_token: crate::config::effective_psk(),
            env: Vec::new(),
        }),
    )?;
    loop {
        match ipc::stream::decode_with_deadline(
            &mut stream,
            std::time::Duration::from_secs(5),
            |r| proto::decode_server(r),
        )? {
            ServerMsg::IdentifyAck { .. } => break,
            ServerMsg::Error { msg } => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
            }
            _ => {}
        }
    }
    proto::send(
        &mut stream,
        &proto::encode_client(&ClientMsg::CaptureWindow {
            session,
            window,
            format,
            colors,
            term_fg: None,
            term_bg: None,
        }),
    )?;
    loop {
        match ipc::stream::decode_with_deadline(
            &mut stream,
            std::time::Duration::from_secs(5),
            |r| proto::decode_server(r),
        )? {
            ServerMsg::WindowCapture { content } => return Ok(content),
            ServerMsg::Error { msg } => return Err(io::Error::other(msg)),
            _ => {}
        }
    }
}

/// Run the client: connect to server, relay stdin → server, render grid updates.
/// If `new_session` is provided, a NewSession command is sent right after the handshake.
/// If `select_session` is provided, a SelectSession command is sent to switch to that session.
pub fn run(
    socket_path: &std::path::Path,
    new_session: Option<Option<String>>,
    select_session: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
) -> io::Result<ClientExit> {
    // Connect to the server (Unix socket, or TCP when --tcp is set).
    let mut stream = ipc::connect_any(socket_path)?;
    let stream_fd = stream.as_raw_fd();

    // Enter raw mode on the controlling terminal.
    let _raw_guard = terminal::enter_raw_mode()?;

    // Enter raw mode. Do NOT use alternate screen — we want the terminal's
    // native scrollback to capture content that scrolls off the top.
    // A scroll region is set (excluding the status bar) so that \x1b[S
    // scrolls only the content area, pushing lines into the terminal's
    // scrollback buffer.
    {
        let mut stdout = client_stdout();
        // Clear screen, hide cursor, set scroll region to exclude status bar.
        let view_rows = terminal::get_size().0.saturating_sub(1);
        write!(stdout, "\x1b[2J\x1b[H\x1b[?25l\x1b[1;{}r", view_rows.max(1))?;
        stdout.flush()?;
    }

    // Get terminal size and send Identify.
    let (rows, cols) = terminal::get_size();
    let identify = proto::encode_client(&ClientMsg::Identify {
        rows,
        cols,
        attach: true,
        auth_token: crate::config::effective_psk(),
        env: terminal_env_overlay(),
    });
    proto::send(&mut stream, &identify)?;

    // Wait for IdentifyAck to get grid dimensions and server version.
    let (grid_rows, grid_cols, server_version) = match proto::decode_server(&mut stream) {
        Ok(ServerMsg::IdentifyAck {
            rows,
            cols,
            version,
            address: _,
        }) => {
            if version != "unknown" && version != version::VERSION {
                eprintln!("\rlrmux: WARNING — server is running a different version: {version}");
            }
            (rows as usize, cols as usize, version)
        }
        Ok(ServerMsg::Error { msg }) => {
            restore_terminal();
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, msg));
        }
        Ok(_) => {
            restore_terminal();
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected IdentifyAck",
            ));
        }
        Err(e) => {
            restore_terminal();
            return Err(e);
        }
    };

    // Filter for terminal replies arriving on stdin — late OSC/CSI/DCS
    // responses to our probes are client-side traffic, not pane input.
    let mut input_filter = input_filter::InputFilter::new();
    // Mouse report decoder — only active while the outer terminal is in
    // a reporting mode (see sync_mouse_modes): reports feed the child
    // (when it tracks the mouse) or local copy-mode selection. Inactive
    // it is a pass-through, so keystrokes see zero added latency.
    let mut mouse_decoder = mouse::Decoder::new();
    // Tracking mask currently applied to the outer terminal (None = not yet).
    let mut applied_mouse: Option<u8> = None;
    // Mouse reporting policy: config mode + Ctrl-A m / [M]-click override
    // + the auto_enable sticky latch.
    let mut mouse_ctl = MouseCtl {
        mode: crate::config::global().behavior.mouse,
        ovr: MouseOverride::Auto,
        latched: false,
    };
    // Pending mouse-press position (x, y): arms a local selection — the
    // drag that follows enters copy mode anchored here. A release without
    // motion is a plain click and does nothing.
    let mut mouse_anchor: Option<(u16, u16)> = None;
    // True while copy mode was entered by a mouse drag (release copies).
    let mut mouse_auto_copy = false;

    // Probe outer TTY defaults (OSC 10/11) so the server can paint HTML
    // captures without waiting for vim to ask. Raw mode is already on —
    // replies won't echo onto the screen. Keystrokes read alongside a
    // reply come back in probe_leftover and re-enter via the filter.
    let mut probe_leftover = Vec::new();
    let mut osc_probe = OscProbe::default();
    report_outer_term_palette(&mut stream, &mut probe_leftover, &mut osc_probe)?;
    input_filter.inject(&probe_leftover);

    // Create the local grid + renderer.
    let mut grid = Grid::new(grid_rows, grid_cols, 10_000);
    let mut renderer = render::Renderer::new(grid_rows, grid_cols);

    // Track the actual terminal size (including status bar row).
    let (mut term_rows, mut term_cols) = {
        let (r, c) = terminal::get_size();
        (r as usize, c as usize)
    };
    // Set initial viewport from terminal size.
    update_viewport(&mut renderer, term_rows, term_cols, grid_rows, grid_cols);

    // Status bar text (updated by the server).
    let mut status_text = String::new();
    // Column spans of each window entry in the status bar (1-based,
    // start inclusive / end exclusive), for click-to-switch hit-testing.
    let mut status_spans: Vec<(u16, u16, u8)> = Vec::new();
    // Current session name (from StatusBarUpdate, used for kill-session confirmation).
    let mut current_session = String::new();
    // Number of windows in the current session (from StatusBarUpdate).
    let mut current_window_count: usize = 0;
    // Number of sessions on the server (from StatusBarUpdate).
    let mut session_count: usize = 0;
    // Last active window index (for Ctrl-A Ctrl-A toggle).
    let mut last_window: Option<u8> = None;
    // Current active window index (tracked from StatusBarUpdate).
    let mut current_window: Option<u8> = None;
    // Temporary status bar message (shown for a few seconds, then cleared).
    let mut flash_msg: Option<String> = None;
    let mut flash_deadline: Option<std::time::Instant> = None;
    let mut pending_session_chooser = false;
    // Last title written to the outer terminal (dedup — status bar updates
    // fire on every output burst but the title rarely changes).
    let mut last_title: Option<String> = None;

    // If requested, create a new session on the server right after handshake.
    // Send the client's CWD so the new session opens in the right directory.
    if let Some(name) = new_session {
        let cwd = cwd.or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });
        let msg = proto::encode_client(&ClientMsg::NewSession {
            name,
            cwd,
            command: command.clone(),
        });
        proto::send(&mut stream, &msg)?;
    } else if let Some(ref cmd) = command {
        // Not creating a new session, but a command was specified.
        // Create a new window with that command.
        let msg = proto::encode_client(&ClientMsg::NewWindowIn {
            session: None,
            command: Some(cmd.clone()),
        });
        proto::send(&mut stream, &msg)?;
    }
    // If requested, switch to an existing session by name.
    if let Some(ref name) = select_session {
        let msg = proto::encode_client(&ClientMsg::SelectSession { name: name.clone() });
        proto::send(&mut stream, &msg)?;
    }

    // Handshake done — the relay loop drains the socket until EAGAIN, so
    // it must be nonblocking or the drain read would freeze the client.
    stream.set_nonblocking(true)?;

    // Clear screen and do initial render.
    {
        let mut stdout = client_stdout();
        stdout.write_all(b"\x1b[2J\x1b[H")?;
        stdout.flush()?;
    }

    // Relay loop: poll stdin + server socket.
    let stdin_fd = io::stdin().as_raw_fd();
    let mut server_buf: Vec<u8> = Vec::new();
    let mut prefix_state = PrefixState::Normal;
    let mut confirm_state = ConfirmState::None;
    // Copy/scrollback mode state (None when inactive).
    let mut copy_mode: Option<copy_mode::CopyMode> = None;
    // Internal paste buffer (for Prefix ] paste).
    let mut paste_buffer = String::new();
    // Reason for exiting the relay loop, printed after terminal restoration.
    let mut exit_reason: Option<String> = None;
    let mut want_selector = false;
    // Set when the exit is the session/server going away (pane exited,
    // server down, socket dropped) rather than the user leaving.
    let mut ended = false;

    // Install SIGWINCH handler so terminal resizes are detected.
    install_winch_handler();

    loop {
        // Check if the terminal was resized.
        // With the viewport model, SIGWINCH does NOT send a resize to the server.
        // The client just re-renders its viewport (crop or filler).
        // Only Prefix F sends an explicit canonical resize to the server.
        if WINCH.swap(false, Ordering::Relaxed) {
            let (new_rows, new_cols) = terminal::get_size();
            term_rows = new_rows as usize;
            term_cols = new_cols as usize;
            client_trace_msg("EVT", &format!("sigwinch term={new_cols}x{new_rows}"));
            // Update scroll region to exclude the status bar.
            {
                let view_rows = term_rows.saturating_sub(1).max(1);
                let mut stdout = client_stdout();
                write!(stdout, "\x1b[1;{}r", view_rows)?;
                stdout.flush()?;
            }
            update_viewport(
                &mut renderer,
                term_rows,
                term_cols,
                grid.rows(),
                grid.cols(),
            );
            if let Some(ref cm) = copy_mode {
                // In copy mode: re-render the copy mode view.
                let view_rows = term_rows.saturating_sub(1);
                let mut stdout = client_stdout();
                cm.render(&mut stdout, &grid, view_rows, term_cols, &status_text)?;
            } else {
                // Clear screen and re-render everything.
                let mut stdout = client_stdout();
                stdout.write_all(b"\x1b[2J\x1b[H")?;
                renderer.invalidate();
                grid.mark_all_dirty();
                renderer.render(&mut stdout, &mut grid)?;
                render_filler(&mut stdout, grid.rows(), grid.cols(), term_rows, term_cols)?;
                render_status_bar(
                    &mut stdout,
                    &status_text,
                    term_rows,
                    term_cols,
                    &grid,
                    mouse_reporting_on(applied_mouse),
                )?;
            }
        }

        let mut fds = [
            libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stream_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        // Poll timeout: 500ms for flash expiry, tightened while the
        // input filter holds a possible partial terminal reply.
        let timeout_ms = input_filter
            .hold_remaining_ms()
            .unwrap_or(500)
            .min(mouse_decoder.hold_remaining_ms().unwrap_or(500))
            .min(500);
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }

        // A held partial sequence that never completed was user input —
        // release it; the staged bytes are picked up below.
        input_filter.flush_expired();
        mouse_decoder.flush_expired();

        // Check if flash message expired.
        if let Some(deadline) = flash_deadline
            && std::time::Instant::now() >= deadline
        {
            flash_msg = None;
            flash_deadline = None;
            // Re-render the normal status bar.
            let mut stdout = client_stdout();
            render_status_bar(
                &mut stdout,
                &status_text,
                term_rows,
                term_cols,
                &grid,
                mouse_reporting_on(applied_mouse),
            )?;
        }

        // If we have a flash message, render it on the status bar.
        if let Some(ref msg) = flash_msg {
            let mut stdout = client_stdout();
            render_flash_status_bar(
                &mut stdout,
                msg,
                &status_text,
                term_rows,
                term_cols,
                &grid,
                mouse_reporting_on(applied_mouse),
            )?;
        }

        // stdin → prefix detection → server (as PaneInput or commands).
        // Also runs when the filter staged bytes without stdin activity —
        // probe leftovers or an expired hold.
        let mut stdin_eof = false;
        if fds[0].revents & libc::POLLIN != 0
            || input_filter.has_staged()
            || mouse_decoder.has_staged()
        {
            let mut raw = Vec::new();
            if fds[0].revents & libc::POLLIN != 0 {
                let mut buf = [0u8; 8192];
                let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n > 0 {
                    raw.extend_from_slice(&buf[..n as usize]);
                    client_trace("KEY", &buf[..n as usize]);
                } else if n == 0 {
                    stdin_eof = true;
                }
            }
            let filtered = input_filter.feed(&raw);
            // Split mouse reports out of the input stream: they are
            // re-encoded for the pane (or handled locally) rather than
            // reaching the child as stray escape bytes.
            let (filtered, mouse_events) = mouse_decoder.feed(&filtered);
            if !filtered.is_empty() {
                client_trace("IN", &filtered);
            }
            if !filtered.is_empty() || !mouse_events.is_empty() {
                let input = &filtered[..];

                // Route decoded mouse events: to the pane when it tracks
                // the mouse, or to local copy-mode selection otherwise
                // (and always locally while copy mode is active).
                let view_rows = term_rows.saturating_sub(1);
                let mut mouse_copy_action: Option<copy_mode::CopyAction> = None;
                // Copy-mode view dirty: drags/wheel arrive in bursts, so
                // render once after the batch instead of per event — a full
                // repaint per motion event is what made selection flicker.
                let mut redraw_cm = false;
                for ev in &mouse_events {
                    // The status bar row is lrmux's own UI — clicks there
                    // never reach the pane (the bounds check below already
                    // drops them), so handling them here is passthrough-safe.
                    if ev.y as usize == term_rows {
                        if ev.is_wheel() {
                            // Wheel on the bar cycles windows, tmux-style.
                            let msg = if ev.is_wheel_up() {
                                ClientMsg::PrevWindow
                            } else {
                                ClientMsg::NextWindow
                            };
                            send_cmd(&mut stream, &msg)?;
                        } else if !ev.release && !ev.is_motion() && ev.cb & 3 == 0 {
                            // The [M] control occupies the last 3 columns —
                            // a click there forces reporting off (clicks
                            // then can't reach us; Ctrl-A m re-enables).
                            if ev.x as usize + 2 >= term_cols && term_cols >= 8 {
                                mouse_ctl.ovr = MouseOverride::ForceOff;
                                flash_msg = Some("mouse off — Ctrl-A m to re-enable".to_string());
                                flash_deadline = Some(
                                    std::time::Instant::now() + std::time::Duration::from_secs(3),
                                );
                                sync_mouse_modes(
                                    &mut applied_mouse,
                                    &grid,
                                    &mut mouse_decoder,
                                    copy_mode.is_some(),
                                    &mut mouse_ctl,
                                );
                                continue;
                            }
                            // Left press on a window entry switches to it.
                            if let Some(&(_, _, idx)) = status_spans
                                .iter()
                                .find(|&&(s, e, _)| ev.x >= s && ev.x < e)
                            {
                                send_cmd(&mut stream, &ClientMsg::SelectWindow { index: idx })?;
                            }
                        }
                        continue;
                    }
                    let local = copy_mode.is_some() || !grid.wants_mouse();
                    if !local {
                        // Forward to the pane in the child's encoding.
                        // Motion events only when the child asked for drag
                        // (1002) or any-motion (1003) reporting.
                        if ev.is_motion() && grid.mouse_tracking & 0b110 == 0 {
                            continue;
                        }
                        if ev.x == 0
                            || ev.x as usize > grid.cols()
                            || ev.y == 0
                            || ev.y as usize > grid.rows().min(view_rows)
                        {
                            continue; // status bar / filler region
                        }
                        let data = mouse::encode_report(grid.mouse_fmt, ev);
                        let msg = proto::encode_client(&ClientMsg::PaneInput { data });
                        proto::send(&mut stream, &msg)?;
                        continue;
                    }
                    // Local handling.
                    if ev.is_wheel() {
                        if grid.mouse_altscroll && !grid.wants_mouse() && copy_mode.is_none() {
                            // DECSET 1007 without tracking: wheel → arrows.
                            let arrow = match (ev.is_wheel_up(), grid.app_cursor_keys) {
                                (true, true) => b"\x1bOA".as_slice(),
                                (true, false) => b"\x1b[A".as_slice(),
                                (false, true) => b"\x1bOB".as_slice(),
                                (false, false) => b"\x1b[B".as_slice(),
                            };
                            // xterm emits three presses per wheel tick.
                            let mut data = Vec::with_capacity(arrow.len() * 3);
                            for _ in 0..3 {
                                data.extend_from_slice(arrow);
                            }
                            let msg = proto::encode_client(&ClientMsg::PaneInput { data });
                            proto::send(&mut stream, &msg)?;
                            continue;
                        }
                        // Inside copy mode the wheel scrolls the copy-mode
                        // view (exits when it reaches the bottom).
                        if let Some(cm) = copy_mode.as_mut() {
                            let total = grid.scrollback.len() + grid.rows();
                            if ev.is_wheel_up() {
                                cm.vrow = cm.vrow.saturating_sub(3);
                            } else {
                                if cm.vrow >= total.saturating_sub(1) {
                                    // Already at the bottom — leave copy mode.
                                    mouse_copy_action = Some(copy_mode::CopyAction::Quit);
                                    break;
                                }
                                cm.vrow = (cm.vrow + 3).min(total - 1);
                            }
                            cm.ensure_cursor_visible(view_rows);
                            redraw_cm = true;
                            continue;
                        }
                        // A wheel report still reached us, so reporting is
                        // on (always_on / auto_enable / forced on) while
                        // this pane doesn't track the mouse — translate
                        // wheel-up into copy-mode scrollback, tmux-style.
                        if ev.is_wheel_up() {
                            let cm = copy_mode.get_or_insert_with(|| {
                                copy_mode::CopyMode::new(
                                    grid.scrollback.len(),
                                    grid.cursor_row,
                                    grid.cursor_col,
                                )
                            });
                            cm.vrow = cm.vrow.saturating_sub(3);
                            cm.ensure_cursor_visible(view_rows);
                            redraw_cm = true;
                        }
                        continue;
                    }
                    if ev.is_motion() {
                        // Drag: arm/extend the local selection.
                        if let Some((ax, ay)) = mouse_anchor.take() {
                            // First motion after a press — enter copy mode
                            // anchored at the press position.
                            if copy_mode.is_none() {
                                copy_mode = Some(copy_mode::CopyMode::new(
                                    grid.scrollback.len(),
                                    grid.cursor_row,
                                    grid.cursor_col,
                                ));
                            }
                            mouse_auto_copy = true;
                            let cm = copy_mode.as_mut().unwrap();
                            cm.vrow = (grid.scrollback.len() + ay.saturating_sub(1) as usize)
                                .min(grid.scrollback.len() + grid.rows() - 1);
                            cm.vcol = (ax.saturating_sub(1) as usize).min(grid.cols() - 1);
                            cm.selection_start = Some((cm.vrow, cm.vcol));
                        }
                        if let Some(cm) = copy_mode.as_mut()
                            && cm.selection_start.is_some()
                        {
                            let before = (cm.vrow, cm.vcol, cm.viewport_top);
                            let total = grid.scrollback.len() + grid.rows();
                            cm.vrow =
                                (cm.viewport_top + ev.y.saturating_sub(1) as usize).min(total - 1);
                            cm.vcol = (ev.x.saturating_sub(1) as usize).min(grid.cols() - 1);
                            cm.ensure_cursor_visible(view_rows);
                            // Skip the redraw when the motion stayed inside
                            // the same cell — nothing changed on screen.
                            if (cm.vrow, cm.vcol, cm.viewport_top) != before {
                                redraw_cm = true;
                            }
                        }
                        continue;
                    }
                    if ev.release {
                        if mouse_auto_copy {
                            // Drag ended — copy the selection and leave.
                            mouse_auto_copy = false;
                            if let Some(cm) = &copy_mode
                                && let Some(text) = cm.copy_selection(&grid)
                                && !text.is_empty()
                            {
                                mouse_copy_action = Some(copy_mode::CopyAction::Copy(text));
                                break;
                            }
                            mouse_copy_action = Some(copy_mode::CopyAction::Quit);
                            break;
                        }
                        // Release without a drag: plain click — cancel the
                        // pending anchor (and any selection if in copy mode).
                        if let Some(cm) = copy_mode.as_mut() {
                            cm.selection_start = None;
                            cm.vrow = (cm.viewport_top + ev.y.saturating_sub(1) as usize)
                                .min(grid.scrollback.len() + grid.rows() - 1);
                            cm.vcol = (ev.x.saturating_sub(1) as usize).min(grid.cols() - 1);
                            cm.ensure_cursor_visible(view_rows);
                            redraw_cm = true;
                        }
                        mouse_anchor = None;
                        continue;
                    }
                    // Button press: arm a selection anchor (drag enters
                    // copy mode; release without drag = plain click).
                    // Left button only.
                    if ev.cb & 3 == 0 {
                        mouse_anchor = Some((ev.x, ev.y));
                    }
                }
                // Apply copy-mode exit/copy triggered by the mouse.
                if let Some(action) = mouse_copy_action {
                    match action {
                        copy_mode::CopyAction::Quit => {
                            copy_mode = None;
                            restore_normal_view(
                                &mut renderer,
                                &mut grid,
                                &status_text,
                                term_rows,
                                term_cols,
                                mouse_reporting_on(applied_mouse),
                            )?;
                        }
                        copy_mode::CopyAction::Copy(text) => {
                            paste_buffer = text.clone();
                            let _ = copy_mode::copy_to_clipboard(&text);
                            copy_mode = None;
                            restore_normal_view(
                                &mut renderer,
                                &mut grid,
                                &status_text,
                                term_rows,
                                term_cols,
                                mouse_reporting_on(applied_mouse),
                            )?;
                        }
                        copy_mode::CopyAction::Continue => {}
                    }
                }
                // One repaint per input batch for copy-mode view changes.
                if redraw_cm && let Some(cm) = copy_mode.as_ref() {
                    let mut stdout = client_stdout();
                    cm.render(&mut stdout, &grid, view_rows, term_cols, &status_text)?;
                }

                if copy_mode.is_some() {
                    // In copy mode: all input goes to copy mode key handling.
                    // Arrow keys send escape sequences (\x1b[A/B/C/D) which must
                    // be distinguished from a standalone Esc (\x1b) that quits.
                    let view_rows = term_rows.saturating_sub(1);
                    let mut copy_action: Option<copy_mode::CopyAction> = None;
                    let mut i = 0;
                    while i < input.len() {
                        if let Some(ref mut cm) = copy_mode {
                            // Check for escape sequence (arrow keys, Home/End, Page Up/Down).
                            if input[i] == 0x1b && i + 2 < input.len() && input[i + 1] == b'[' {
                                let handled = match input[i + 2] {
                                    b'A' => {
                                        cm.move_cursor(-1, 0, &grid);
                                        cm.ensure_cursor_visible(view_rows);
                                        true
                                    }
                                    b'B' => {
                                        cm.move_cursor(1, 0, &grid);
                                        cm.ensure_cursor_visible(view_rows);
                                        true
                                    }
                                    b'C' => {
                                        cm.move_cursor(0, 1, &grid);
                                        true
                                    }
                                    b'D' => {
                                        cm.move_cursor(0, -1, &grid);
                                        true
                                    }
                                    b'H' => {
                                        cm.vcol = 0;
                                        true
                                    } // Home
                                    b'F' => {
                                        cm.vcol = cm.last_non_blank(&grid, cm.vrow);
                                        true
                                    } // End
                                    _ => false,
                                };
                                if handled {
                                    i += 3;
                                    let mut stdout = client_stdout();
                                    cm.render(
                                        &mut stdout,
                                        &grid,
                                        view_rows,
                                        term_cols,
                                        &status_text,
                                    )?;
                                    continue;
                                }
                            }
                            // Check for Page Up/Down: \x1b[5~ or \x1b[6~
                            if input[i] == 0x1b && i + 3 < input.len() && input[i + 1] == b'[' {
                                let handled = match (input[i + 2], input[i + 3]) {
                                    (b'5', b'~') => {
                                        let page = view_rows.max(1);
                                        cm.move_cursor(-(page as i32), 0, &grid);
                                        cm.ensure_cursor_visible(view_rows);
                                        true
                                    }
                                    (b'6', b'~') => {
                                        let page = view_rows.max(1);
                                        cm.move_cursor(page as i32, 0, &grid);
                                        cm.ensure_cursor_visible(view_rows);
                                        true
                                    }
                                    _ => false,
                                };
                                if handled {
                                    i += 4;
                                    let mut stdout = client_stdout();
                                    cm.render(
                                        &mut stdout,
                                        &grid,
                                        view_rows,
                                        term_cols,
                                        &status_text,
                                    )?;
                                    continue;
                                }
                            }
                            // Standalone Esc (not followed by [) → quit copy mode.
                            // Esc at the end of buffer is also treated as quit.
                            if input[i] == 0x1b && (i + 1 >= input.len() || input[i + 1] != b'[') {
                                copy_action = Some(copy_mode::CopyAction::Quit);
                                break;
                            }
                            // \x1b[ followed by unknown byte — skip the \x1b and let
                            // the normal byte-by-byte handler deal with the rest.
                            let action = cm.process_key(input[i], &grid, view_rows);
                            match action {
                                copy_mode::CopyAction::Continue => {
                                    let mut stdout = client_stdout();
                                    cm.render(
                                        &mut stdout,
                                        &grid,
                                        view_rows,
                                        term_cols,
                                        &status_text,
                                    )?;
                                }
                                _ => {
                                    copy_action = Some(action);
                                    break;
                                }
                            }
                        }
                        i += 1;
                    }
                    // Handle copy mode exit (outside the borrow).
                    if let Some(action) = copy_action {
                        match action {
                            copy_mode::CopyAction::Quit => {
                                copy_mode = None;
                                restore_normal_view(
                                    &mut renderer,
                                    &mut grid,
                                    &status_text,
                                    term_rows,
                                    term_cols,
                                    mouse_reporting_on(applied_mouse),
                                )?;
                            }
                            copy_mode::CopyAction::Copy(text) => {
                                paste_buffer = text.clone();
                                let _ = copy_mode::copy_to_clipboard(&text);
                                copy_mode = None;
                                restore_normal_view(
                                    &mut renderer,
                                    &mut grid,
                                    &status_text,
                                    term_rows,
                                    term_cols,
                                    mouse_reporting_on(applied_mouse),
                                )?;
                            }
                            copy_mode::CopyAction::Continue => {}
                        }
                    }
                } else if matches!(confirm_state, ConfirmState::None) {
                    let (
                        passthrough,
                        detach,
                        to_selector,
                        confirm,
                        remaining,
                        enter_copy_mode,
                        paste,
                        mut flash,
                        show_help,
                        request_session_chooser,
                        toggle_mouse,
                    ) = process_prefix(
                        input,
                        &mut prefix_state,
                        &mut stream,
                        current_window_count,
                        session_count,
                        last_window,
                    )?;
                    if toggle_mouse {
                        mouse_ctl.ovr = mouse_ctl.ovr.next();
                        flash = Some(match mouse_ctl.ovr {
                            MouseOverride::Auto => "mouse: auto (config)".to_string(),
                            MouseOverride::ForceOn => "mouse: forced ON".to_string(),
                            MouseOverride::ForceOff => {
                                "mouse: forced OFF (clicks dead — Ctrl-A m cycles)".to_string()
                            }
                        });
                        // Apply before the flash render below so the [M]
                        // button repaints in the same pass.
                        sync_mouse_modes(
                            &mut applied_mouse,
                            &grid,
                            &mut mouse_decoder,
                            copy_mode.is_some(),
                            &mut mouse_ctl,
                        );
                    }
                    if show_help {
                        show_help_overlay(&server_version);
                        // Invalidate the renderer so the screen is fully redrawn.
                        renderer.invalidate();
                        // Re-establish the scroll region and re-render.
                        let view_rows = term_rows.saturating_sub(1);
                        let mut stdout = client_stdout();
                        write!(stdout, "\x1b[1;{}r", view_rows.max(1)).ok();
                        renderer.render(&mut stdout, &mut grid)?;
                        render_status_bar(
                            &mut stdout,
                            &status_text,
                            term_rows,
                            term_cols,
                            &grid,
                            mouse_reporting_on(applied_mouse),
                        )?;
                        // Ask the server for a fresh snapshot too — a plain
                        // re-render can leave stale rows after the overlay
                        // cleared the screen.
                        send_cmd(&mut stream, &ClientMsg::Refresh)?;
                    }
                    if request_session_chooser {
                        pending_session_chooser = true;
                    }
                    if let Some(msg) = flash {
                        flash_msg = Some(msg);
                        flash_deadline =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                        let mut stdout = client_stdout();
                        render_flash_status_bar(
                            &mut stdout,
                            flash_msg.as_ref().unwrap(),
                            &status_text,
                            term_rows,
                            term_cols,
                            &grid,
                            mouse_reporting_on(applied_mouse),
                        )?;
                    }
                    if !passthrough.is_empty() {
                        let msg = proto::encode_client(&ClientMsg::PaneInput { data: passthrough });
                        proto::send(&mut stream, &msg)?;
                    }
                    if detach {
                        let msg = proto::encode_client(&ClientMsg::Detach);
                        proto::send(&mut stream, &msg)?;
                        if to_selector {
                            want_selector = true;
                        } else {
                            exit_reason = Some("detached".to_string());
                        }
                        break;
                    }
                    if enter_copy_mode {
                        copy_mode = Some(copy_mode::CopyMode::new(
                            grid.scrollback.len(),
                            grid.cursor_row,
                            grid.cursor_col,
                        ));
                        let view_rows = term_rows.saturating_sub(1);
                        let mut stdout = client_stdout();
                        copy_mode.as_ref().unwrap().render(
                            &mut stdout,
                            &grid,
                            view_rows,
                            term_cols,
                            &status_text,
                        )?;
                    }
                    if paste && !paste_buffer.is_empty() {
                        let msg = proto::encode_client(&ClientMsg::PaneInput {
                            data: paste_buffer.as_bytes().to_vec(),
                        });
                        proto::send(&mut stream, &msg)?;
                    }
                    if let Some(mut c) = confirm {
                        // Fill in the session name and window count for kill-session confirmation.
                        if let ConfirmState::KillSession {
                            target,
                            window_count,
                            ..
                        } = &mut c
                        {
                            target.clone_from(&current_session);
                            *window_count = current_window_count;
                        }
                        // Pre-fill rename prompt with the current session name.
                        if let ConfirmState::RenameSession { input } = &mut c {
                            input.clone_from(&current_session);
                        }
                        confirm_state = c;
                        render_confirm_prompt(&confirm_state, term_rows);
                        // Process any remaining bytes that arrived after the
                        // confirm trigger in the same read buffer.
                        if !remaining.is_empty() {
                            let action =
                                process_confirm(&remaining, &mut confirm_state, &mut stream)?;
                            match action {
                                ConfirmAction::Confirmed | ConfirmAction::Cancelled => {
                                    confirm_state = ConfirmState::None;
                                    let mut stdout = client_stdout();
                                    render_status_bar(
                                        &mut stdout,
                                        &status_text,
                                        term_rows,
                                        term_cols,
                                        &grid,
                                        mouse_reporting_on(applied_mouse),
                                    )?;
                                }
                                ConfirmAction::Continue => {
                                    render_confirm_prompt(&confirm_state, term_rows);
                                }
                            }
                        }
                    }
                } else {
                    // In confirm mode: all input goes to the confirm handler.
                    let action = process_confirm(input, &mut confirm_state, &mut stream)?;
                    match action {
                        ConfirmAction::Confirmed => {
                            confirm_state = ConfirmState::None;
                            if let Some(msg) = take_network_setup_flash() {
                                flash_msg = Some(msg);
                                flash_deadline = Some(
                                    std::time::Instant::now() + std::time::Duration::from_secs(8),
                                );
                            }
                            let mut stdout = client_stdout();
                            render_status_bar(
                                &mut stdout,
                                &status_text,
                                term_rows,
                                term_cols,
                                &grid,
                                mouse_reporting_on(applied_mouse),
                            )?;
                        }
                        ConfirmAction::Cancelled => {
                            confirm_state = ConfirmState::None;
                            let mut stdout = client_stdout();
                            render_status_bar(
                                &mut stdout,
                                &status_text,
                                term_rows,
                                term_cols,
                                &grid,
                                mouse_reporting_on(applied_mouse),
                            )?;
                        }
                        ConfirmAction::Continue => {
                            render_confirm_prompt(&confirm_state, term_rows);
                        }
                    }
                }
            }
        }
        // Copy mode toggles via input — keep outer mouse reporting in
        // sync (it backs drag-select while copy mode is active).
        if sync_mouse_modes(
            &mut applied_mouse,
            &grid,
            &mut mouse_decoder,
            copy_mode.is_some(),
            &mut mouse_ctl,
        ) {
            // The [M] indicator changed state — repaint the bar.
            let mut stdout = client_stdout();
            if let Some(ref msg) = flash_msg {
                render_flash_status_bar(
                    &mut stdout,
                    msg,
                    &status_text,
                    term_rows,
                    term_cols,
                    &grid,
                    mouse_reporting_on(applied_mouse),
                )?;
            } else {
                render_status_bar(
                    &mut stdout,
                    &status_text,
                    term_rows,
                    term_cols,
                    &grid,
                    mouse_reporting_on(applied_mouse),
                )?;
            }
        }
        if stdin_eof {
            break;
        }

        // Server → grid → renderer → stdout
        if fds[1].revents & libc::POLLIN != 0 {
            // Drain the socket fully (bounded) and render once per batch —
            // a burst arrives as many frames in one read, and rendering per
            // frame is what makes the client fall behind.
            let mut buf = [0u8; 65536];
            let mut total = 0usize;
            // 0 = EOF, -1 = EAGAIN or error — checked after parsing.
            let mut last: isize = -1;
            loop {
                let n = match stream.read(&mut buf) {
                    Ok(n) => n as isize,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => -1,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => 0,
                };
                if n > 0 {
                    total += n as usize;
                    server_buf.extend_from_slice(&buf[..n as usize]);
                    if total < 4 * 1024 * 1024 {
                        continue;
                    }
                    // Keep the event loop responsive; poll refires.
                    break;
                }
                last = n;
                break;
            }
            if total > 0 {
                let mut needs_render = false;
                while let Some(msg) = try_parse_server_frame(&mut server_buf)? {
                    match msg {
                        ServerMsg::ScrollbackUpdate { rows, replay } => {
                            // Push to internal scrollback (for copy mode).
                            let n = rows.len();
                            client_trace_msg(
                                "EVT",
                                &format!(
                                    "scrollback +{n}rows replay={replay} total={}",
                                    grid.scrollback.len() + n
                                ),
                            );
                            for row in rows {
                                grid.scrollback.push(row);
                            }
                            // Scroll the terminal to push content into the
                            // terminal's native scrollback buffer — but only
                            // for live scroll. Replay chunks (history after a
                            // GridSnapshot) would scroll the just-rendered
                            // content off screen, leaving it blank. The same
                            // applies while in copy mode: the scroll would
                            // push the viewed rows away.
                            if n > 0 && copy_mode.is_none() && !replay {
                                let mut stdout = client_stdout();
                                write!(stdout, "\x1b[{}S", n)?;
                                stdout.flush()?;
                                // The terminal shifted all content up by n lines.
                                // Mark all rows dirty so the renderer rewrites them
                                // at their correct positions (the GridUpdate that
                                // follows will render them).
                                grid.mark_all_dirty();
                                renderer.invalidate();
                            }
                        }
                        ServerMsg::GridUpdate {
                            dirty,
                            cursor_row,
                            cursor_col,
                            cursor_visible,
                            mouse_flags,
                        } => {
                            for (row, cells) in &dirty {
                                apply_row(&mut grid, *row as usize, cells);
                            }
                            grid.cursor_row = cursor_row as usize;
                            grid.cursor_col = cursor_col as usize;
                            grid.cursor_visible = cursor_visible;
                            grid.set_mouse_flags(mouse_flags);
                            client_trace_msg(
                                "EVT",
                                &format!(
                                    "grid-update dirty={}rows cur={cursor_row};{cursor_col} vis={cursor_visible}",
                                    dirty.len()
                                ),
                            );
                            needs_render = true;
                        }
                        ServerMsg::GridSnapshot {
                            rows,
                            cols,
                            cells,
                            cursor_row,
                            cursor_col,
                            cursor_visible,
                            mouse_flags,
                        } => {
                            grid = Grid::new(rows as usize, cols as usize, 10_000);
                            grid.mark_all_dirty();
                            renderer.resize(rows as usize, cols as usize);
                            update_viewport(
                                &mut renderer,
                                term_rows,
                                term_cols,
                                rows as usize,
                                cols as usize,
                            );
                            renderer.invalidate();
                            let cols = cols as usize;
                            for (i, cell) in cells.iter().enumerate() {
                                let row = i / cols;
                                let col = i % cols;
                                if row < grid.rows()
                                    && let Some(r) = grid.row_mut(row)
                                    && col < r.len()
                                {
                                    r[col] = cell.clone();
                                }
                            }
                            grid.cursor_row = cursor_row as usize;
                            grid.cursor_col = cursor_col as usize;
                            grid.cursor_visible = cursor_visible;
                            grid.set_mouse_flags(mouse_flags);
                            client_trace_msg(
                                "EVT",
                                &format!("snapshot {rows}x{cols} cur={cursor_row};{cursor_col}"),
                            );
                            let mut stdout = client_stdout();
                            // Reset scroll region to full screen, clear, then
                            // re-establish the scroll region. This ensures the
                            // clear affects the entire screen and the terminal
                            // viewport is in a clean state before rendering.
                            let view_rows = term_rows.saturating_sub(1);
                            write!(stdout, "\x1b[r\x1b[2J\x1b[H\x1b[1;{}r", view_rows.max(1))?;
                            renderer.render(&mut stdout, &mut grid)?;
                            render_filler(
                                &mut stdout,
                                grid.rows(),
                                grid.cols(),
                                term_rows,
                                term_cols,
                            )?;
                            render_status_bar(
                                &mut stdout,
                                &status_text,
                                term_rows,
                                term_cols,
                                &grid,
                                mouse_reporting_on(applied_mouse),
                            )?;
                        }
                        ServerMsg::StatusBarUpdate {
                            session,
                            windows,
                            active,
                            session_count: sc,
                            high_output: h,
                            server,
                            activity,
                            mouse,
                        } => {
                            // Track last window for Ctrl-A Ctrl-A toggle.
                            let new_active = Some(active as u8);
                            if new_active != current_window && current_window.is_some() {
                                last_window = current_window;
                            }
                            current_window = new_active;
                            current_session = session.clone();
                            current_window_count = windows.len();
                            session_count = sc as usize;
                            (status_text, status_spans) = format_status_bar(
                                &session,
                                &windows,
                                active as usize,
                                &server_version,
                                h,
                                &server,
                                &activity,
                                &mouse,
                            );
                            let mut stdout = client_stdout();
                            if let Some(ref msg) = flash_msg {
                                render_flash_status_bar(
                                    &mut stdout,
                                    msg,
                                    &status_text,
                                    term_rows,
                                    term_cols,
                                    &grid,
                                    mouse_reporting_on(applied_mouse),
                                )?;
                            } else {
                                render_status_bar(
                                    &mut stdout,
                                    &status_text,
                                    term_rows,
                                    term_cols,
                                    &grid,
                                    mouse_reporting_on(applied_mouse),
                                )?;
                            }
                            // Outer-terminal (iTerm tab/window) title:
                            // "lrmux - server:session:window", ●-prefixed
                            // while any window has unseen output.
                            let title = outer_title(
                                &server,
                                &session,
                                &windows,
                                active as usize,
                                &activity,
                            );
                            if last_title.as_deref() != Some(title.as_str()) {
                                write!(stdout, "\x1b]0;{title}\x07")?;
                                stdout.flush()?;
                                last_title = Some(title);
                            }
                        }
                        ServerMsg::PaneExit { .. } => {
                            exit_reason = Some("session ended (last pane exited)".to_string());
                            ended = true;
                            break;
                        }
                        ServerMsg::IdentifyAck { .. } => {}
                        ServerMsg::SessionList {
                            sessions,
                            address: _,
                        } => {
                            if pending_session_chooser {
                                pending_session_chooser = false;
                                let names: Vec<String> =
                                    sessions.iter().map(|s| s.name.clone()).collect();
                                if let Some(name) = show_session_chooser(&names) {
                                    let msg = proto::encode_client(&ClientMsg::SelectSession {
                                        name: name.clone(),
                                    });
                                    let _ = proto::send(&mut stream, &msg);
                                }
                                // Invalidate the renderer so the screen is fully redrawn.
                                renderer.invalidate();
                                let view_rows = term_rows.saturating_sub(1);
                                let mut stdout = client_stdout();
                                write!(stdout, "\x1b[1;{}r", view_rows.max(1)).ok();
                                renderer.render(&mut stdout, &mut grid)?;
                            }
                        }
                        ServerMsg::WindowCapture { .. } => {}
                        ServerMsg::LogContent { lines } => {
                            show_log_overlay(&lines);
                            // Invalidate the renderer so the screen is fully redrawn.
                            renderer.invalidate();
                            // Re-establish the scroll region and re-render.
                            let view_rows = term_rows.saturating_sub(1);
                            let mut stdout = client_stdout();
                            write!(stdout, "\x1b[1;{}r", view_rows.max(1)).ok();
                            renderer.render(&mut stdout, &mut grid)?;
                        }
                        ServerMsg::Error { msg } => {
                            exit_reason = Some(format!("server error: {msg}"));
                            break;
                        }
                        ServerMsg::ControlNotify { .. } => {
                            // Control mode notifications are only for control clients.
                            // The interactive client ignores them.
                        }
                        ServerMsg::TermOscQuery {
                            pane_id,
                            code,
                            bell_terminated,
                        } => {
                            // Child asked for the real terminal's fg/bg color.
                            // Flush any pending render bytes first so the OSC
                            // query isn't stuck behind a partial stdout write.
                            let _ = client_stdout().flush();
                            let mut probe_leftover = Vec::new();
                            if let Some(reply) = query_outer_osc_color(
                                code,
                                bell_terminated,
                                &mut probe_leftover,
                                &mut osc_probe,
                            ) {
                                let msg = proto::encode_client(&ClientMsg::TermOscReply {
                                    pane_id,
                                    data: reply,
                                });
                                proto::send(&mut stream, &msg)?;
                            }
                            input_filter.inject(&probe_leftover);
                        }
                        ServerMsg::PskUpdated => {
                            flash_msg = Some("PSK updated on server".to_string());
                            flash_deadline =
                                Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
                        }
                        // Directory messages are answered by CLI paths, not
                        // inside an attached session.
                        ServerMsg::PeerList { .. }
                        | ServerMsg::RegisterAck { .. }
                        | ServerMsg::RelayAck { .. }
                        | ServerMsg::SendKeysAck { .. } => {}
                    }
                }
                // GridUpdate/Snapshot carry the child's mouse flags — keep
                // the outer terminal's reporting modes in sync.
                sync_mouse_modes(
                    &mut applied_mouse,
                    &grid,
                    &mut mouse_decoder,
                    copy_mode.is_some(),
                    &mut mouse_ctl,
                );
                // Render once per socket batch instead of once per frame —
                // during a burst many GridUpdates arrive in a single read.
                if needs_render && exit_reason.is_none() {
                    client_trace_msg(
                        "EVT",
                        &format!("render cur={};{}", grid.cursor_row, grid.cursor_col),
                    );
                    if let Some(ref cm) = copy_mode {
                        let view_rows = term_rows.saturating_sub(1);
                        let mut stdout = client_stdout();
                        cm.render(&mut stdout, &grid, view_rows, term_cols, &status_text)?;
                    } else {
                        let mut stdout = client_stdout();
                        renderer.render(&mut stdout, &mut grid)?;
                        render_status_bar(
                            &mut stdout,
                            &status_text,
                            term_rows,
                            term_cols,
                            &grid,
                            mouse_reporting_on(applied_mouse),
                        )?;
                    }
                }
            } else if last == 0 {
                // Server closed the connection (EOF).
                ended = true;
                let sock_path = socket_path.to_string_lossy();
                if !std::path::Path::new(&*sock_path).exists() {
                    exit_reason = Some("server shut down".to_string());
                } else {
                    let log_hint = server_log_path(socket_path);
                    exit_reason = Some(format!(
                        "disconnected from server (socket still present — server may have crashed, or this client was dropped for backpressure). Check log at {log_hint}"
                    ));
                }
                break;
            } else {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::WouldBlock {
                    exit_reason = Some(format!("read error from server: {err}"));
                    break;
                }
            }
        }

        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            // stdin closed — terminal gone, just exit.
            break;
        }
        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            // Server socket hung up. Check if the server is still alive
            // to give the user a clue about why we disconnected.
            ended = true;
            let sock_path = socket_path.to_string_lossy();
            if !std::path::Path::new(&*sock_path).exists() {
                exit_reason = Some("server shut down".to_string());
            } else {
                let log_hint = server_log_path(socket_path);
                exit_reason = Some(format!(
                    "disconnected from server (socket still present — server may have crashed, or this client was dropped for backpressure). Check log at {log_hint}"
                ));
            }
            break;
        }
    }

    restore_terminal();
    // Drop the raw mode guard before printing the exit reason so the
    // terminal is in cooked mode (ONLCR) and newlines work normally.
    drop(_raw_guard);
    if let Some(reason) = exit_reason {
        eprintln!("lrmux: {reason}");
    }
    let hint = || {
        let tcp = crate::ipc::tcp_addr();
        SelectHint {
            server: if tcp.is_none() {
                socket_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            } else {
                None
            },
            tcp,
            session: if current_session.is_empty() {
                None
            } else {
                Some(current_session)
            },
        }
    };
    Ok(if want_selector {
        ClientExit::Selector(hint())
    } else if ended {
        ClientExit::SessionEnded(hint())
    } else {
        ClientExit::Done
    })
}

/// Log file for a server socket at `/tmp/lrmux-<UID>/<name>` lives at
/// `/tmp/lrmux-<UID>/logs/<name>.log` (not `<socket>.log`).
fn server_log_path(socket_path: &std::path::Path) -> String {
    let name = socket_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    match socket_path.parent() {
        Some(dir) => dir
            .join("logs")
            .join(format!("{name}.log"))
            .display()
            .to_string(),
        None => format!("{name}.log"),
    }
}

/// Process input bytes through the prefix-key state machine.
/// Returns (passthrough, detach, to_selector, confirm, remaining,
/// enter_copy_mode, paste, flash, show_help, request_session_chooser,
/// toggle_mouse).
/// When a confirm dialog is triggered, remaining bytes after the trigger are returned
/// so the caller can process them with process_confirm.
#[allow(clippy::type_complexity)]
fn process_prefix(
    input: &[u8],
    state: &mut PrefixState,
    stream: &mut ipc::ConnStream,
    window_count: usize,
    session_count: usize,
    last_window: Option<u8>,
) -> io::Result<(
    Vec<u8>,
    bool,
    bool,
    Option<ConfirmState>,
    Vec<u8>,
    bool,
    bool,
    Option<String>,
    bool,
    bool,
    bool,
)> {
    let mut passthrough: Vec<u8> = Vec::new();
    let mut detach = false;
    let mut to_selector = false;
    let mut confirm: Option<ConfirmState> = None;
    let mut enter_copy_mode = false;
    let mut paste = false;
    let mut flash: Option<String> = None;
    let mut show_help = false;
    let mut request_session_chooser = false;
    let mut toggle_mouse = false;

    for (i, &byte) in input.iter().enumerate() {
        if confirm.is_some() {
            // Stop processing — return remaining bytes for the confirm handler.
            return Ok((
                passthrough,
                detach,
                to_selector,
                confirm,
                input[i..].to_vec(),
                enter_copy_mode,
                paste,
                flash,
                show_help,
                request_session_chooser,
                toggle_mouse,
            ));
        }
        match state {
            PrefixState::Normal => {
                if byte == PREFIX {
                    *state = PrefixState::Command;
                } else {
                    passthrough.push(byte);
                }
            }
            PrefixState::Command => {
                match byte {
                    // Double prefix → toggle to last focused window.
                    PREFIX => {
                        if let Some(idx) = last_window {
                            send_cmd(stream, &ClientMsg::SelectWindow { index: idx })?;
                        } else {
                            // No last window — send literal prefix to child.
                            passthrough.push(PREFIX);
                        }
                    }
                    // 'a' → send literal Ctrl-A to the child (screen/byobu).
                    b'a' => {
                        passthrough.push(PREFIX);
                    }
                    // 'c' → new window.
                    b'c' => {
                        send_cmd(stream, &ClientMsg::NewWindow)?;
                    }
                    // 'n', Space, or Ctrl-Space → next window.
                    b'n' | b' ' | 0x00 | 0x0e => {
                        if window_count <= 1 {
                            flash = Some("No next window".to_string());
                        } else {
                            send_cmd(stream, &ClientMsg::NextWindow)?;
                        }
                    }
                    // 'p' or Ctrl-P → previous window.
                    b'p' | 0x10 => {
                        if window_count <= 1 {
                            flash = Some("No previous window".to_string());
                        } else {
                            send_cmd(stream, &ClientMsg::PrevWindow)?;
                        }
                    }
                    // 'd' or Ctrl-D → detach (handled by caller after passthrough is sent).
                    b'd' | 0x04 => {
                        detach = true;
                    }
                    // '/' → detach and return to the session/server selector.
                    b'/' => {
                        detach = true;
                        to_selector = true;
                    }
                    // 'x' → kill pane (no confirmation, immediate).
                    b'x' => {
                        send_cmd(stream, &ClientMsg::KillPane)?;
                    }
                    // 'k' → kill current window (with y/n confirmation).
                    b'k' => {
                        confirm = Some(ConfirmState::KillWindow);
                    }
                    // 'K' → kill current session (type name to confirm).
                    b'K' => {
                        confirm = Some(ConfirmState::KillSession {
                            input: String::new(),
                            target: String::new(),
                            window_count: 0,
                        });
                    }
                    // '$' → rename current session (editable name prompt).
                    b'$' => {
                        confirm = Some(ConfirmState::RenameSession {
                            input: String::new(),
                        });
                    }
                    // 'C' → new session (uppercase, like lowercase 'c' for new window).
                    b'C' => {
                        send_cmd(
                            stream,
                            &ClientMsg::NewSession {
                                name: None,
                                cwd: None,
                                command: None,
                            },
                        )?;
                    }
                    // 'N' → next session.
                    b'N' => {
                        if session_count <= 1 {
                            flash = Some("No next session".to_string());
                        } else {
                            send_cmd(stream, &ClientMsg::NextSession)?;
                        }
                    }
                    // 'P' → previous session.
                    b'P' => {
                        if session_count <= 1 {
                            flash = Some("No previous session".to_string());
                        } else {
                            send_cmd(stream, &ClientMsg::PrevSession)?;
                        }
                    }
                    // 'S' → session chooser (list sessions, pick one).
                    b'S' => {
                        send_cmd(stream, &ClientMsg::ListSessions)?;
                        request_session_chooser = true;
                    }
                    // 'F' → explicit canonical resize (resize panes to current terminal size).
                    b'F' => {
                        let (r, c) = terminal::get_size();
                        send_cmd(stream, &ClientMsg::Resize { rows: r, cols: c })?;
                    }
                    // 'm' → cycle the mouse-reporting override
                    // (auto → forced on → forced off → auto).
                    b'm' => {
                        toggle_mouse = true;
                    }
                    // '[' → enter copy/scrollback mode.
                    b'[' => {
                        enter_copy_mode = true;
                    }
                    // ']' → paste from internal paste buffer.
                    b']' => {
                        paste = true;
                    }
                    // '\' → show server ring log overlay.
                    b'\\' => {
                        send_cmd(stream, &ClientMsg::GetLog)?;
                    }
                    // 'r' → refresh the screen with a fresh snapshot from the server.
                    b'r' => {
                        send_cmd(stream, &ClientMsg::Refresh)?;
                        flash = Some("refreshing...".to_string());
                    }
                    // ',' → network / PSK setup (scaffolding; fullscreen editor later).
                    b',' => {
                        confirm = Some(ConfirmState::NetworkSetupMenu);
                    }
                    // '?' → show keybindings help overlay.
                    b'?' => {
                        show_help = true;
                    }
                    // '0'–'9' → select window by index.
                    b'0'..=b'9' => {
                        send_cmd(stream, &ClientMsg::SelectWindow { index: byte - b'0' })?;
                    }
                    // Unknown command → discarded.
                    _ => {}
                }
                *state = PrefixState::Normal;
            }
        }
    }

    Ok((
        passthrough,
        detach,
        to_selector,
        confirm,
        Vec::new(),
        enter_copy_mode,
        paste,
        flash,
        show_help,
        request_session_chooser,
        toggle_mouse,
    ))
}

/// Send a command message to the server.
fn send_cmd(stream: &mut ipc::ConnStream, msg: &ClientMsg) -> io::Result<()> {
    let encoded = proto::encode_client(msg);
    proto::send(stream, &encoded)
}

/// Result of processing input in a confirm dialog.
enum ConfirmAction {
    /// User confirmed the action (command already sent).
    Confirmed,
    /// User cancelled (n, Esc, Ctrl-C).
    Cancelled,
    /// Still typing — need more input.
    Continue,
}

/// Process input bytes during a confirmation dialog.
fn process_confirm(
    input: &[u8],
    state: &mut ConfirmState,
    stream: &mut ipc::ConnStream,
) -> io::Result<ConfirmAction> {
    match state {
        ConfirmState::None => Ok(ConfirmAction::Cancelled),
        ConfirmState::KillWindow => {
            // Single-key confirmation: y = kill, anything else = cancel.
            match input.first() {
                Some(b'y' | b'Y') => {
                    send_cmd(stream, &ClientMsg::KillPane)?;
                    Ok(ConfirmAction::Confirmed)
                }
                _ => Ok(ConfirmAction::Cancelled),
            }
        }
        ConfirmState::KillSession {
            input: buf, target, ..
        } => {
            for &byte in input {
                match byte {
                    // Enter → check if typed name matches target.
                    b'\r' | b'\n' => {
                        if buf == target {
                            send_cmd(stream, &ClientMsg::KillSession)?;
                            return Ok(ConfirmAction::Confirmed);
                        }
                        return Ok(ConfirmAction::Cancelled);
                    }
                    // Esc or Ctrl-C → cancel.
                    0x1b | 0x03 => {
                        return Ok(ConfirmAction::Cancelled);
                    }
                    // Backspace → remove last char.
                    0x7f | 0x08 => {
                        buf.pop();
                    }
                    // Printable ASCII → append.
                    0x20..=0x7e => {
                        buf.push(byte as char);
                    }
                    _ => {}
                }
            }
            Ok(ConfirmAction::Continue)
        }
        ConfirmState::RenameSession { input: buf } => {
            for &byte in input {
                match byte {
                    // Enter → send rename command.
                    b'\r' | b'\n' => {
                        if !buf.is_empty() {
                            send_cmd(stream, &ClientMsg::RenameSession { name: buf.clone() })?;
                            return Ok(ConfirmAction::Confirmed);
                        }
                        return Ok(ConfirmAction::Cancelled);
                    }
                    // Esc or Ctrl-C → cancel.
                    0x1b | 0x03 => {
                        return Ok(ConfirmAction::Cancelled);
                    }
                    // Backspace → remove last char.
                    0x7f | 0x08 => {
                        buf.pop();
                    }
                    // Printable ASCII → append.
                    0x20..=0x7e => {
                        buf.push(byte as char);
                    }
                    _ => {}
                }
            }
            Ok(ConfirmAction::Continue)
        }
        ConfirmState::NetworkSetupMenu => match input.first() {
            Some(b'g' | b'G') => {
                let psk = crate::config::generate_psk()?;
                crate::config::persist_psk(&psk)?;
                send_cmd(stream, &ClientMsg::SetPsk { psk: psk.clone() })?;
                // Show the full PSK once so it can be shared with remotes.
                *state = ConfirmState::None;
                // Re-use flash via a side channel: return Confirmed and let
                // caller set flash — encode PSK in a temporary Rename? Better:
                // stash via environment-less approach: write to a static.
                NETWORK_SETUP_FLASH.lock().unwrap().replace(format!(
                    "PSK set — share once: {psk} (TLS required outside safe_networks)"
                ));
                Ok(ConfirmAction::Confirmed)
            }
            Some(b's' | b'S') => {
                *state = ConfirmState::NetworkSetupSetPsk {
                    input: String::new(),
                };
                Ok(ConfirmAction::Continue)
            }
            Some(0x1b | 0x03 | b'q' | b'Q') => Ok(ConfirmAction::Cancelled),
            _ => Ok(ConfirmAction::Continue),
        },
        ConfirmState::NetworkSetupSetPsk { input: buf } => {
            for &byte in input {
                match byte {
                    b'\r' | b'\n' => {
                        if buf.is_empty() {
                            return Ok(ConfirmAction::Cancelled);
                        }
                        let psk = buf.clone();
                        crate::config::persist_psk(&psk)?;
                        send_cmd(stream, &ClientMsg::SetPsk { psk: psk.clone() })?;
                        NETWORK_SETUP_FLASH.lock().unwrap().replace(format!(
                            "PSK set ({} chars). Remotes: --psk + --tcp",
                            psk.len()
                        ));
                        return Ok(ConfirmAction::Confirmed);
                    }
                    0x1b | 0x03 => return Ok(ConfirmAction::Cancelled),
                    0x7f | 0x08 => {
                        buf.pop();
                    }
                    0x20..=0x7e => {
                        buf.push(byte as char);
                    }
                    _ => {}
                }
            }
            Ok(ConfirmAction::Continue)
        }
    }
}

fn take_network_setup_flash() -> Option<String> {
    NETWORK_SETUP_FLASH.lock().unwrap().take()
}

/// Render the confirmation prompt on the status bar line.
fn render_confirm_prompt(state: &ConfirmState, term_rows: usize) {
    let mut stdout = client_stdout();
    let row = term_rows;
    // Reset SGR before clear/write so reverse/underline from the pane
    // cannot bleed into the prompt (same issue as the status bar).
    write!(stdout, "\x1b[0m\x1b[{};1H\x1b[2K", row).ok();
    match state {
        ConfirmState::None => {}
        ConfirmState::KillWindow => {
            write!(stdout, "\x1b[0;43;30m Kill current window? (y/n) \x1b[0m").ok();
        }
        ConfirmState::KillSession {
            input,
            target,
            window_count,
        } => {
            write!(
                stdout,
                "\x1b[0;41;97m Kill session '{}' ({} window{})? Type the name to confirm: {}\x1b[0m",
                target,
                window_count,
                if *window_count == 1 { "" } else { "s" },
                input
            )
            .ok();
        }
        ConfirmState::RenameSession { input } => {
            write!(
                stdout,
                "\x1b[0;44;97m Rename session: {}\x1b[0;1;44;93m_\x1b[0;44;97m  (Enter=confirm, Esc=cancel)\x1b[0m",
                input
            )
            .ok();
        }
        ConfirmState::NetworkSetupMenu => {
            let has = !crate::config::effective_psk().is_empty();
            write!(
                stdout,
                "\x1b[0;44;97m Network setup: [g]enerate PSK  [s]et PSK  [q]uit{}\x1b[0m",
                if has { " (PSK already set)" } else { "" }
            )
            .ok();
        }
        ConfirmState::NetworkSetupSetPsk { input } => {
            write!(
                stdout,
                "\x1b[0;44;97m Enter PSK: {}\x1b[0;1;44;93m_\x1b[0;44;97m  (Enter=save, Esc=cancel)\x1b[0m",
                "*".repeat(input.len())
            )
            .ok();
        }
    }
    stdout.flush().ok();
}

/// Format the status bar text with colors.
/// The bar uses a blue background; the active window is highlighted in bold yellow.
/// Inactive windows with pending activity get a red bullet prefix.
/// Identity is `[session]@server`, then the window list.
/// Also returns each window entry's 1-based column span (start, end,
/// window index) for mouse hit-testing on the status bar row.
#[allow(clippy::too_many_arguments)]
fn format_status_bar(
    session: &str,
    windows: &[String],
    active: usize,
    server_version: &str,
    high_output: bool,
    server: &str,
    activity: &[bool],
    mouse: &[bool],
) -> (String, Vec<(u16, u16, u8)>) {
    // Each sequence starts with `0;` so reverse/underline/italic from the
    // pane cannot leak into the bar (AI TUIs often leave SGR 4/7 active).
    // Blue background + white text for inactive windows.
    const BAR: &str = "\x1b[0;44;97m"; // reset, bg blue, bright white
    // Active window: bold bright yellow on blue.
    const ACTIVE: &str = "\x1b[0;1;44;93m"; // reset, bold, bg blue, bright yellow
    // Session name: bold bright cyan on blue.
    const SESSION: &str = "\x1b[0;1;44;96m"; // reset, bold, bg blue, bright cyan
    // Activity bullet: bold bright red on blue.
    const ACTIVITY: &str = "\x1b[0;1;44;91m";
    const RESET: &str = "\x1b[0m";
    const WARN: &str = "\x1b[0;1;44;31m"; // reset, bold red on blue

    let server_hash = server_version.rsplit('-').next().unwrap_or(server_version);
    let mismatch = server_version != version::VERSION;
    let version_marker = if mismatch {
        // Restore BAR after the bang so the blue background continues.
        format!("{WARN}!{BAR}")
    } else {
        String::new()
    };
    let burst_marker = if high_output {
        format!("{WARN}[BURST]{BAR} ")
    } else {
        String::new()
    };

    // Per-window mouse marker: cyan [M] on blue when that window's pane
    // requested mouse tracking.
    const MWANT: &str = "\x1b[0;44;96m";

    let mut parts: Vec<String> = Vec::new();
    for (i, name) in windows.iter().enumerate() {
        let has_act = activity.get(i).copied().unwrap_or(false);
        let m = if mouse.get(i).copied().unwrap_or(false) {
            format!("{MWANT}[M]{BAR}")
        } else {
            String::new()
        };
        if i == active {
            parts.push(format!(
                "{}{}:{}*{}{}{}",
                ACTIVE, i, name, BAR, m, burst_marker
            ));
        } else if has_act {
            parts.push(format!("{ACTIVITY}●{BAR}{}:{}{}", i, name, m));
        } else {
            parts.push(format!("{}:{}{}", i, name, m));
        }
    }
    let identity = if server.is_empty() {
        format!("{SESSION}{session}{BAR}")
    } else {
        format!("[{SESSION}{session}{BAR}]@{server}")
    };

    // Visible layout: "lrmux <hash><marker> | <identity> | <parts…>".
    // Track each window entry's column span while composing so mouse
    // clicks on the status bar row can be hit-tested against it.
    let head_visible = "lrmux ".chars().count()
        + server_hash.chars().count()
        + usize::from(mismatch)
        + " | ".chars().count()
        + strip_ansi(&identity).chars().count()
        + " | ".chars().count();
    let mut spans: Vec<(u16, u16, u8)> = Vec::with_capacity(windows.len());
    let mut col = head_visible as u16 + 1;
    for (i, part) in parts.iter().enumerate() {
        let w = strip_ansi(part).chars().count() as u16;
        spans.push((col, col + w, i as u8));
        col += w + 3; // entries are joined with " | "
    }

    (
        format!(
            "{}lrmux {}{}{} | {} | {}{}",
            BAR,
            version_marker,
            server_hash,
            BAR,
            identity,
            parts.join(" | "),
            RESET
        ),
        spans,
    )
}

/// Compose the outer-terminal (iTerm tab/window) title shown while
/// attached: `lrmux - server:session:window`, prefixed with `● ` when any
/// window has unseen output — the same flags that drive the status-bar
/// bullets. Names are user-controlled (rename-session/window), so control
/// characters are stripped to keep them out of the OSC payload.
fn outer_title(
    server: &str,
    session: &str,
    windows: &[String],
    active: usize,
    activity: &[bool],
) -> String {
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let window = windows.get(active).map(String::as_str).unwrap_or("?");
    let dot = if activity.iter().any(|a| *a) {
        "● "
    } else {
        ""
    };
    format!(
        "{dot}lrmux - {}:{}:{}",
        clean(server),
        clean(session),
        clean(window)
    )
}

/// Render the status bar at the bottom of the terminal.
/// The status bar always occupies the last row of the terminal (term_rows).
/// After rendering, the cursor is repositioned to the grid cursor location.
fn render_status_bar(
    stdout: &mut TraceStdout,
    text: &str,
    term_rows: usize,
    term_cols: usize,
    grid: &Grid,
    mouse_on: bool,
) -> io::Result<()> {
    // Position cursor at the last row of the terminal (1-based).
    let row = term_rows;
    // Reset SGR *before* clear/write. The diff renderer leaves the last
    // cell's attrs active (AI CLIs often use reverse/underline), and
    // `\x1b[2K` / bare `\x1b[44m` would otherwise inherit them — making
    // the footer look inverted or underlined until the next clean redraw.
    write!(stdout, "\x1b[0m\x1b[{};1H\x1b[2K", row)?;
    // Write the full status bar text (it includes its own ANSI colors).
    stdout.write_all(text.as_bytes())?;
    // Measure visible width (excluding ANSI escape sequences) and pad
    // the rest of the line with the bar background color so the
    // blue background extends to the right edge of the terminal.
    let max_cols = term_cols.min(500);
    let visible_len = strip_ansi(text).chars().count();
    if visible_len < max_cols {
        // Full SGR reset + blue bg (no reverse/underline/bold).
        write!(stdout, "\x1b[0;44m{}", " ".repeat(max_cols - visible_len))?;
    }
    // Reset attributes.
    stdout.write_all(b"\x1b[0m")?;
    // [M] indicator/toggle at the right edge.
    draw_mouse_button(stdout, term_rows, term_cols, mouse_on)?;
    // Reposition cursor to the grid cursor location so the user sees
    // the cursor in the pane, not on the status bar.
    // Only reposition if the cursor is within the viewport.
    if grid.cursor_visible {
        let view_rows = grid.rows().min(term_rows.saturating_sub(1));
        let view_cols = grid.cols().min(term_cols);
        if grid.cursor_row < view_rows && grid.cursor_col < view_cols {
            write!(
                stdout,
                "\x1b[{};{}H",
                grid.cursor_row + 1,
                grid.cursor_col + 1
            )?;
        }
    }
    stdout.flush()?;
    Ok(())
}

/// Render the status bar with a flash message appended on the right.
fn render_flash_status_bar(
    stdout: &mut TraceStdout,
    msg: &str,
    normal_text: &str,
    term_rows: usize,
    term_cols: usize,
    grid: &Grid,
    mouse_on: bool,
) -> io::Result<()> {
    let row = term_rows;
    write!(stdout, "\x1b[0m\x1b[{};1H\x1b[2K", row)?;
    let max_cols = term_cols.min(500);

    // Strip escape sequences from normal_text to measure visible width.
    let normal_visible: String = strip_ansi(normal_text);
    let normal_len = normal_visible.chars().count();

    // Flash message in bold yellow on blue, with a separator.
    // Leading `0;` clears reverse/underline inherited from the pane.
    const FLASH: &str = "\x1b[0;1;44;93m"; // reset, bold, bg blue, bright yellow
    const BAR: &str = "\x1b[0;44;97m"; // reset, bg blue, bright white

    let flash_text = format!("{}  ⚠ {}{}", FLASH, msg, BAR);
    let flash_visible: String = strip_ansi(&flash_text);
    let flash_len = flash_visible.chars().count();

    // If both fit, show normal on left + flash on right.
    if normal_len + flash_len <= max_cols {
        // Write normal status bar (it already has its own colors).
        let normal_display: String = normal_text.chars().take(max_cols).collect();
        stdout.write_all(normal_display.as_bytes())?;
        // Write flash message.
        stdout.write_all(flash_text.as_bytes())?;
    } else {
        // Not enough room — just show the flash message.
        let display: String = flash_text.chars().take(max_cols).collect();
        stdout.write_all(display.as_bytes())?;
    }

    // Pad the rest with blue background.
    let total_visible = normal_len + flash_len;
    if total_visible < max_cols {
        write!(stdout, "\x1b[0;44m{}", " ".repeat(max_cols - total_visible))?;
    }
    stdout.write_all(b"\x1b[0m")?;
    draw_mouse_button(stdout, term_rows, term_cols, mouse_on)?;

    // Reposition cursor.
    if grid.cursor_visible {
        let view_rows = grid.rows().min(term_rows.saturating_sub(1));
        let view_cols = grid.cols().min(term_cols);
        if grid.cursor_row < view_rows && grid.cursor_col < view_cols {
            write!(
                stdout,
                "\x1b[{};{}H",
                grid.cursor_row + 1,
                grid.cursor_col + 1
            )?;
        }
    }
    stdout.flush()?;
    Ok(())
}

/// Strip ANSI escape sequences from a string to measure visible width.
fn strip_ansi(s: &str) -> String {
    let mut result = String::new();
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            // End of escape sequence: letter (e.g. 'm', 'H', 'K', 'r', 'S', 'J')
            if ch.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if ch == '\x1b' {
            in_esc = true;
        } else {
            result.push(ch);
        }
    }
    result
}

/// Update the renderer's viewport based on terminal and grid dimensions.
fn update_viewport(
    renderer: &mut render::Renderer,
    term_rows: usize,
    term_cols: usize,
    grid_rows: usize,
    grid_cols: usize,
) {
    let view_rows = grid_rows.min(term_rows.saturating_sub(1));
    let view_cols = grid_cols.min(term_cols);
    renderer.set_viewport(view_rows, view_cols);
}

/// Restore the normal (non-copy-mode) view after exiting copy mode.
fn restore_normal_view(
    renderer: &mut render::Renderer,
    grid: &mut Grid,
    status_text: &str,
    term_rows: usize,
    term_cols: usize,
    mouse_on: bool,
) -> io::Result<()> {
    let mut stdout = client_stdout();
    // Hide cursor (copy mode shows it; normal mode hides it).
    stdout.write_all(b"\x1b[?25l")?;
    // Reset scroll region to exclude status bar, then clear and re-render.
    let view_rows = term_rows.saturating_sub(1).max(1);
    write!(stdout, "\x1b[1;{}r\x1b[2J\x1b[H", view_rows)?;
    renderer.invalidate();
    grid.mark_all_dirty();
    renderer.render(&mut stdout, grid)?;
    render_filler(&mut stdout, grid.rows(), grid.cols(), term_rows, term_cols)?;
    render_status_bar(
        &mut client_stdout(),
        status_text,
        term_rows,
        term_cols,
        grid,
        mouse_on,
    )?;
    Ok(())
}

/// Render the filler region: the area beyond the canonical grid when the
/// terminal is larger than the grid. Uses a dim background with a thin
/// border line separating content from filler (per §2.16 of the design doc).
fn render_filler(
    stdout: &mut TraceStdout,
    grid_rows: usize,
    grid_cols: usize,
    term_rows: usize,
    term_cols: usize,
) -> io::Result<()> {
    let content_rows = term_rows.saturating_sub(1); // minus status bar
    let content_cols = term_cols;

    // Filler color: dim dark gray background.
    const FILLER_BG: &str = "\x1b[48;5;236m";
    const BORDER: &str = "\x1b[90m"; // bright black (gray)
    const RESET: &str = "\x1b[0m";

    let has_bottom_filler = grid_rows < content_rows;
    let has_right_filler = grid_cols < content_cols;

    // 1. Fill columns to the right of the grid (in visible grid rows).
    if has_right_filler {
        for row in 1..=grid_rows.min(content_rows) {
            // Filler background for the right side.
            write!(stdout, "\x1b[{};{}H{}", row, grid_cols + 1, FILLER_BG)?;
            for _ in grid_cols..content_cols {
                write!(stdout, " ")?;
            }
        }
        // Vertical border between grid and filler columns.
        if grid_cols > 0 {
            for row in 1..=grid_rows.min(content_rows) {
                write!(
                    stdout,
                    "\x1b[{};{}H{}│{}",
                    row,
                    grid_cols + 1,
                    BORDER,
                    RESET
                )?;
            }
        }
    }

    // 2. Fill rows below the grid (between grid and status bar).
    if has_bottom_filler {
        // Horizontal border row: draw ─ across the grid width.
        write!(stdout, "\x1b[{};1H{}", grid_rows + 1, BORDER)?;
        let h_border_cols = grid_cols.min(content_cols);
        for _ in 0..h_border_cols {
            write!(stdout, "─")?;
        }
        // At the corner where horizontal and vertical borders meet, draw ┘.
        // Then fill the rest of the border row with filler background.
        if has_right_filler && grid_cols > 0 && grid_cols < content_cols {
            write!(
                stdout,
                "\x1b[{};{}H{}┘{}",
                grid_rows + 1,
                grid_cols + 1,
                BORDER,
                FILLER_BG
            )?;
            for _ in (grid_cols + 1)..content_cols {
                write!(stdout, " ")?;
            }
        } else if content_cols > grid_cols {
            // No right filler border, just fill with filler background.
            write!(stdout, "{}", FILLER_BG)?;
            for _ in grid_cols..content_cols {
                write!(stdout, " ")?;
            }
        }

        // Filler rows below the border: fill full width with filler background.
        // No vertical border here — the corner ┘ already closes the border.
        for row in (grid_rows + 2)..=content_rows {
            write!(stdout, "\x1b[{};1H\x1b[2K{}", row, FILLER_BG)?;
            for _ in 0..content_cols {
                write!(stdout, " ")?;
            }
        }
    }

    stdout.write_all(RESET.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// Apply a row of cells to the local grid.
fn apply_row(grid: &mut Grid, row: usize, cells: &[Cell]) {
    if row >= grid.rows() {
        return;
    }
    if let Some(r) = grid.row_mut(row) {
        for (col, cell) in cells.iter().enumerate() {
            if col >= r.len() {
                break;
            }
            r[col] = cell.clone();
        }
    }
    grid.mark_dirty(row);
}

/// Restore the terminal (exit alternate screen, show cursor).
fn restore_terminal() {
    let mut stdout = client_stdout();
    // Disable mouse reporting we may have enabled, reset scroll region
    // to full screen, show cursor, clear screen.
    let _ = stdout.write_all(mouse::terminal_teardown().as_bytes());
    let _ = stdout.write_all(b"\x1b[r\x1b[?25h\x1b[2J\x1b[H");
    let _ = stdout.flush();
}

/// Runtime mouse override toggled by `Ctrl-A m` or a click on the
/// status-bar [M] control (which can only disable — when reporting is
/// off, clicks never reach us).
#[derive(Clone, Copy, PartialEq, Eq)]
enum MouseOverride {
    /// Follow the configured `behavior.mouse` policy.
    Auto,
    /// Reporting on regardless of pane requests.
    ForceOn,
    /// Reporting off regardless of pane requests.
    ForceOff,
}

impl MouseOverride {
    fn next(self) -> Self {
        match self {
            Self::Auto => Self::ForceOn,
            Self::ForceOn => Self::ForceOff,
            Self::ForceOff => Self::Auto,
        }
    }
}

/// Outer-terminal mouse policy state: config mode + runtime override +
/// the auto_enable sticky latch.
struct MouseCtl {
    mode: crate::config::MouseMode,
    ovr: MouseOverride,
    latched: bool,
}

/// The effective outer-terminal mouse mask.
///
/// Base is the child's tracking mask, widened to button-events (1002)
/// when something local needs reports — copy mode selection or the
/// altscroll wheel translation. The configured mode can force that on
/// (`always_on`, or `auto_enable` after the first pane request) or fully
/// off (`off`); the runtime override always wins last.
fn desired_mouse_mask(ctl: &mut MouseCtl, grid: &Grid, local: bool) -> u8 {
    use crate::config::MouseMode;
    if ctl.mode == MouseMode::AutoEnable && grid.mouse_tracking != 0 {
        ctl.latched = true;
    }
    // Runtime override wins over the configured mode in both directions.
    if ctl.ovr == MouseOverride::ForceOff {
        return 0;
    }
    if ctl.mode == MouseMode::Off && ctl.ovr != MouseOverride::ForceOn {
        return 0;
    }
    let mut desired = grid.mouse_tracking;
    if local || (grid.mouse_altscroll && desired == 0) {
        desired |= 0b010;
    }
    if ctl.mode == MouseMode::AlwaysOn
        || (ctl.mode == MouseMode::AutoEnable && ctl.latched)
        || ctl.ovr == MouseOverride::ForceOn
    {
        desired |= 0b010;
    }
    desired
}

/// Keep the outer terminal's mouse reporting in sync with what can use
/// reports: the child's tracking mask, alternate-scroll (wheel→arrows
/// translation needs wheel reports), or copy mode (drag-select) — all
/// under the configured policy and runtime override. With none of those,
/// reporting stays off and the terminal keeps its native wheel
/// scrollback and selection — lrmux never interprets the wheel.
/// Returns true when the applied mask changed (status bar repaint).
fn sync_mouse_modes(
    applied: &mut Option<u8>,
    grid: &Grid,
    decoder: &mut mouse::Decoder,
    local: bool,
    ctl: &mut MouseCtl,
) -> bool {
    let desired = desired_mouse_mask(ctl, grid, local);
    if *applied == Some(desired) {
        return false;
    }
    let mut stdout = client_stdout();
    let _ = stdout.write_all(mouse::terminal_teardown().as_bytes());
    if desired != 0 {
        let _ = stdout.write_all(mouse::terminal_setup(desired).as_bytes());
    }
    let _ = stdout.flush();
    *applied = Some(desired);
    decoder.set_active(desired != 0);
    true
}

/// Whether outer-terminal mouse reports can currently reach us.
fn mouse_reporting_on(applied: Option<u8>) -> bool {
    applied.unwrap_or(0) != 0
}

/// Paint the `[M]`/`[m]` mouse-reporting indicator at the status bar's
/// right edge: bright green while reports reach lrmux (clickable —
/// disables), dim while off (clicks can't reach us then; `Ctrl-A m`
/// re-enables).
fn draw_mouse_button(
    stdout: &mut TraceStdout,
    term_rows: usize,
    term_cols: usize,
    on: bool,
) -> io::Result<()> {
    if term_cols < 8 {
        return Ok(());
    }
    let (style, label) = if on {
        ("\x1b[0;1;44;92m", "[M]")
    } else {
        ("\x1b[0;2;44;37m", "[m]")
    };
    write!(
        stdout,
        "\x1b[{};{}H{}{}\x1b[0m",
        term_rows,
        term_cols - 2,
        style,
        label
    )?;
    Ok(())
}

/// Try to parse a complete server frame from the buffer.
fn try_parse_server_frame(buf: &mut Vec<u8>) -> io::Result<Option<ServerMsg>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let frame: Vec<u8> = buf.drain(..4 + len).collect();
    let mut cursor = io::Cursor::new(frame);
    let msg = proto::decode_server(&mut cursor)?;
    Ok(Some(msg))
}

/// Show the server ring log as a temporary overlay.
/// Waits for any key to dismiss.
fn show_log_overlay(lines: &[String]) {
    use std::io::Read;
    let mut stdout = client_stdout();
    let (rows, cols) = terminal::get_size();

    // Clear screen and show the log.
    write!(stdout, "\x1b[2J\x1b[H\x1b[?25h").ok();
    stdout.flush().ok();

    let title = "lrmux server log (press any key to dismiss)";
    write!(stdout, "\x1b[1;36m{title}\x1b[0m\r\n").ok();
    write!(
        stdout,
        "\x1b[90m{}\x1b[0m\r\n",
        "─".repeat(cols.saturating_sub(1) as usize)
    )
    .ok();

    let available = rows.saturating_sub(3) as usize;
    let start = if lines.len() > available {
        lines.len() - available
    } else {
        0
    };
    for line in &lines[start..] {
        // Truncate to terminal width.
        let truncated: String = line.chars().take(cols.saturating_sub(1) as usize).collect();
        write!(stdout, "{truncated}\r\n").ok();
    }

    stdout.flush().ok();

    // Wait for a single keypress to dismiss.
    let mut buf = [0u8; 1];
    let _ = std::io::stdin().read(&mut buf);

    // Clear and request a full re-render.
    write!(stdout, "\x1b[2J\x1b[H\x1b[?25l").ok();
    stdout.flush().ok();
}

/// Show a session chooser overlay.
/// Returns the selected session name, or None if cancelled.
fn show_session_chooser(sessions: &[String]) -> Option<String> {
    use std::io::Read;
    let mut stdout = client_stdout();
    let (rows, _cols) = terminal::get_size();

    write!(stdout, "\x1b[2J\x1b[H\x1b[?25h").ok();
    stdout.flush().ok();

    let title = "lrmux sessions (number to switch, any other key to cancel)";
    write!(stdout, "\x1b[1;36m{title}\x1b[0m\r\n").ok();
    write!(stdout, "\r\n").ok();

    let available = rows.saturating_sub(4) as usize;
    for (i, name) in sessions.iter().take(available).enumerate() {
        write!(stdout, "  \x1b[1;33m{i}\x1b[0m  {name}\r\n").ok();
    }

    stdout.flush().ok();

    // Wait for a single keypress.
    let mut buf = [0u8; 1];
    if std::io::stdin().read(&mut buf).is_ok() && buf[0] >= b'0' && buf[0] <= b'9' {
        let idx = (buf[0] - b'0') as usize;
        if idx < sessions.len() {
            // Clear and request a full re-render.
            write!(stdout, "\x1b[2J\x1b[H\x1b[?25l").ok();
            stdout.flush().ok();
            return Some(sessions[idx].clone());
        }
    }

    // Clear and request a full re-render.
    write!(stdout, "\x1b[2J\x1b[H\x1b[?25l").ok();
    stdout.flush().ok();
    None
}

/// Show the keybindings help as a temporary overlay.
/// Waits for any key to dismiss.
fn show_help_overlay(server_version: &str) {
    use std::io::Read;
    let mut stdout = client_stdout();
    let (rows, _cols) = terminal::get_size();

    write!(stdout, "\x1b[2J\x1b[H\x1b[?25h").ok();
    stdout.flush().ok();

    let title = "lrmux keybindings (press any key to dismiss)";
    write!(stdout, "\x1b[1;36m{title}\x1b[0m\r\n").ok();
    write!(
        stdout,
        "  client: \x1b[33m{}\x1b[0m  server: \x1b[33m{}\x1b[0m\r\n",
        version::VERSION,
        server_version
    )
    .ok();
    write!(stdout, "\r\n").ok();

    let bindings = [
        ("Ctrl-A c", "New window"),
        ("Ctrl-A n / Space / Ctrl-Space", "Next window"),
        ("Ctrl-A p / Ctrl-P", "Previous window"),
        ("Ctrl-A Ctrl-A", "Toggle last focused window"),
        ("Ctrl-A a", "Send Ctrl-A to pane"),
        ("Ctrl-A 0-9", "Select window by index"),
        ("Ctrl-A C", "New session"),
        ("Ctrl-A N", "Next session"),
        ("Ctrl-A P", "Previous session"),
        ("Ctrl-A $", "Rename session"),
        ("Ctrl-A K", "Kill session (confirm)"),
        ("Ctrl-A x", "Kill pane/window"),
        ("Ctrl-A k", "Kill pane (confirm)"),
        ("Ctrl-A [", "Enter copy mode"),
        ("Ctrl-A ]", "Paste from buffer"),
        ("Ctrl-A F", "Resize to terminal size"),
        ("Ctrl-A r", "Refresh screen"),
        ("Ctrl-A ,", "Network / PSK setup"),
        ("Ctrl-A d / Ctrl-D", "Detach"),
        ("Ctrl-A /", "Detach to session selector"),
        ("Ctrl-A \\", "Show server log"),
        ("Ctrl-A ?", "Show this help"),
    ];

    for (key, desc) in &bindings {
        write!(stdout, "  \x1b[1;33m{key:<35}\x1b[0m {desc}\r\n").ok();
    }

    // Ensure we don't overflow.
    if rows as usize > bindings.len() + 4 {
        write!(stdout, "\r\n").ok();
    }

    stdout.flush().ok();

    // Wait for a single keypress to dismiss.
    let mut buf = [0u8; 1];
    let _ = std::io::stdin().read(&mut buf);

    // Clear and request a full re-render.
    write!(stdout, "\x1b[2J\x1b[H\x1b[?25l").ok();
    stdout.flush().ok();
}

/// Query OSC 10/11 on the outer TTY and send `TermPalette` to the server.
/// Non-reply bytes picked up while probing (user keystrokes read
/// alongside the reply) go into `extra_input` for the caller to re-feed
/// as input instead of dropping them.
/// How long an OSC probe waits for a reply per channel. Real terminals
/// answer in a few ms; 300ms still covers laggy SSH links while a silent
/// terminal costs a fraction of a second instead of the old 2s per
/// attempt — which stacked across channels and colors into a multi-second
/// attach freeze (e.g. on tablet terminals that don't answer OSC at all).
const OSC_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// Liveness cache for the outer-TTY query channels. A channel that timed
/// out once is marked dead: a terminal that doesn't answer OSC won't
/// start answering on the next probe, and retrying would re-freeze the
/// input loop for the whole timeout on every subsequent query.
#[derive(Default)]
pub(crate) struct OscProbe {
    /// stdin/stdout channel (interactive attach only).
    stdio: Option<bool>,
    /// /dev/tty channel — fallback for the interactive client, and the
    /// only channel for -CC where stdout is the control-mode stream.
    dev_tty: Option<bool>,
}

impl OscProbe {
    fn alive(known: Option<bool>) -> bool {
        known != Some(false)
    }
}

pub(crate) fn report_outer_term_palette(
    stream: &mut impl Write,
    extra_input: &mut Vec<u8>,
    probe: &mut OscProbe,
) -> io::Result<()> {
    // Both queries go out in one write per channel — the wait covers both
    // replies, halving the worst-case block on slow or silent terminals.
    let mut fg = None;
    let mut bg = None;
    let take = |code: u8, res: &mut Vec<Option<Vec<u8>>>| {
        let i = if code == 10 { 0 } else { 1 };
        res[i]
            .take()
            .and_then(|d| crate::term::parse_osc_color_reply(&d))
            .map(|(_, rgb)| rgb)
    };
    if OscProbe::alive(probe.stdio) {
        let mut res = query_osc_colors_on_fds(
            libc::STDIN_FILENO,
            libc::STDOUT_FILENO,
            &[10, 11],
            true,
            extra_input,
        );
        probe.stdio = Some(res.iter().any(Option::is_some));
        fg = take(10, &mut res);
        bg = take(11, &mut res);
    }
    if (fg.is_none() || bg.is_none()) && OscProbe::alive(probe.dev_tty) {
        let codes: Vec<u8> = [(fg.is_none()).then_some(10), (bg.is_none()).then_some(11)]
            .into_iter()
            .flatten()
            .collect();
        let mut res = query_osc_colors_via_dev_tty(&codes, true, extra_input);
        probe.dev_tty = Some(res.iter().any(Option::is_some));
        for (code, r) in codes.iter().zip(res.iter_mut()) {
            let rgb = r
                .take()
                .and_then(|d| crate::term::parse_osc_color_reply(&d))
                .map(|(_, rgb)| rgb);
            match code {
                10 => fg = rgb,
                _ => bg = rgb,
            }
        }
    }
    if fg.is_none() && bg.is_none() {
        return Ok(());
    }
    let msg = proto::encode_client(&ClientMsg::TermPalette { fg, bg });
    proto::send(stream, &msg)
}

/// Query the outer terminal's OSC 10 (fg) or 11 (bg) color.
///
/// Tries stdout/stdin first (the fds the interactive client already has on
/// the user's terminal), then `/dev/tty` as a fallback — unless `probe`
/// already knows that channel is dead. A failed query surfaces as vim
/// E1568 and wrong `background` / colorscheme colors.
pub(crate) fn query_outer_osc_color(
    code: u8,
    bell_terminated: bool,
    extra_input: &mut Vec<u8>,
    probe: &mut OscProbe,
) -> Option<Vec<u8>> {
    if code != 10 && code != 11 {
        return None;
    }
    if OscProbe::alive(probe.stdio) {
        let mut res = query_osc_colors_on_fds(
            libc::STDIN_FILENO,
            libc::STDOUT_FILENO,
            &[code],
            bell_terminated,
            extra_input,
        );
        probe.stdio = Some(res[0].is_some());
        if let Some(reply) = res[0].take() {
            return Some(reply);
        }
    }
    if OscProbe::alive(probe.dev_tty) {
        let mut res = query_osc_colors_via_dev_tty(&[code], bell_terminated, extra_input);
        probe.dev_tty = Some(res[0].is_some());
        if let Some(reply) = res[0].take() {
            return Some(reply);
        }
    }
    None
}

/// Control-mode variant: never write OSC to stdout (that's the tmux control
/// channel). Only query via `/dev/tty`.
pub(crate) fn query_outer_osc_color_for_control(
    code: u8,
    bell_terminated: bool,
    extra_input: &mut Vec<u8>,
    probe: &mut OscProbe,
) -> Option<Vec<u8>> {
    if code != 10 && code != 11 || !OscProbe::alive(probe.dev_tty) {
        return None;
    }
    let mut res = query_osc_colors_via_dev_tty(&[code], bell_terminated, extra_input);
    probe.dev_tty = Some(res[0].is_some());
    res[0].take()
}

fn query_osc_colors_via_dev_tty(
    codes: &[u8],
    bell_terminated: bool,
    extra_input: &mut Vec<u8>,
) -> Vec<Option<Vec<u8>>> {
    let fd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    if fd < 0 {
        return vec![None; codes.len()];
    }
    struct TtyFd(i32);
    impl Drop for TtyFd {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.0);
            }
        }
    }
    let tty = TtyFd(fd);
    query_osc_colors_on_fds(tty.0, tty.0, codes, bell_terminated, extra_input)
}

/// Returns one slot per queried code — the OSC reply bytes, or None on
/// timeout for that code. Bytes read that are not part of a reply —
/// pending input drained up front, user keystrokes arriving mid-wait,
/// trailing bytes after the last reply — are appended to `extra_input`
/// so the caller can re-feed them instead of swallowing what the user
/// typed while we were blocked probing.
fn query_osc_colors_on_fds(
    read_fd: i32,
    write_fd: i32,
    codes: &[u8],
    bell_terminated: bool,
    extra_input: &mut Vec<u8>,
) -> Vec<Option<Vec<u8>>> {
    // Disable ECHO while waiting for the reply. Interactive clients are
    // already raw, but control-mode / edge paths can still be cooked — and
    // an echoed OSC reply paints garbage on the user's screen.
    let mut saved_termios = None;
    if unsafe { libc::isatty(read_fd) } != 0 {
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(read_fd) };
        if let Ok(orig) = nix::sys::termios::tcgetattr(borrowed) {
            let mut quiet = orig.clone();
            quiet.local_flags.remove(
                nix::sys::termios::LocalFlags::ECHO
                    | nix::sys::termios::LocalFlags::ECHOE
                    | nix::sys::termios::LocalFlags::ECHOK
                    | nix::sys::termios::LocalFlags::ECHONL,
            );
            if nix::sys::termios::tcsetattr(borrowed, nix::sys::termios::SetArg::TCSANOW, &quiet)
                .is_ok()
            {
                saved_termios = Some(orig);
            }
        }
    }
    struct RestoreEcho(Option<(i32, nix::sys::termios::Termios)>);
    impl Drop for RestoreEcho {
        fn drop(&mut self) {
            if let Some((fd, ref orig)) = self.0 {
                let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
                let _ = nix::sys::termios::tcsetattr(
                    borrowed,
                    nix::sys::termios::SetArg::TCSANOW,
                    orig,
                );
            }
        }
    }
    let _echo_guard = RestoreEcho(saved_termios.map(|t| (read_fd, t)));

    // Drain pending input so leftover key bytes aren't mistaken for the
    // reply — but keep them: they are real user input, not garbage.
    let fl = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
    if fl >= 0 {
        unsafe {
            libc::fcntl(read_fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
    {
        let mut tmp = [0u8; 256];
        loop {
            let n = unsafe { libc::read(read_fd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
            if n > 0 {
                extra_input.extend_from_slice(&tmp[..n as usize]);
                client_trace("KEY", &tmp[..n as usize]);
                continue;
            }
            break;
        }
    }

    let mut query = Vec::with_capacity(16 * codes.len());
    for code in codes {
        query.extend_from_slice(b"\x1b]");
        query.extend_from_slice(code.to_string().as_bytes());
        query.extend_from_slice(b";?");
        if bell_terminated {
            query.push(0x07);
        } else {
            query.extend_from_slice(b"\x1b\\");
        }
    }
    let w = unsafe { libc::write(write_fd, query.as_ptr() as *const _, query.len()) };
    client_trace("QRY", &query);
    if w < 0 {
        if fl >= 0 {
            unsafe {
                libc::fcntl(read_fd, libc::F_SETFL, fl);
            }
        }
        return vec![None; codes.len()];
    }
    if write_fd == libc::STDOUT_FILENO {
        let _ = client_stdout().flush();
    }

    let mut replies: Vec<Option<Vec<u8>>> = vec![None; codes.len()];
    let mut buf = Vec::with_capacity(128);
    let mut tmp = [0u8; 256];
    let deadline = std::time::Instant::now() + OSC_REPLY_TIMEOUT;
    if fl >= 0 {
        unsafe {
            libc::fcntl(read_fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
    loop {
        if replies.iter().all(Option::is_some) || std::time::Instant::now() >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let mut pfd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().min(50) as i32;
        let pret = unsafe { libc::poll(&mut pfd, 1, ms) };
        if pret <= 0 {
            continue;
        }
        let n = unsafe { libc::read(read_fd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
        if n <= 0 {
            continue;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
        client_trace("KEY", &tmp[..n as usize]);
        // Extract every complete reply; map each to its query by OSC code.
        while let Some(start) = buf.windows(2).position(|w| w == b"\x1b]") {
            let Some(end) = osc_reply_end(&buf[start..]).map(|e| start + e) else {
                break;
            };
            extra_input.extend_from_slice(&buf[..start]);
            let reply = buf[start..end].to_vec();
            if let Some((code, _)) = crate::term::parse_osc_color_reply(&reply)
                && let Some(slot) = codes
                    .iter()
                    .zip(replies.iter_mut())
                    .find(|(c, r)| **c == code && r.is_none())
                    .map(|(_, r)| r)
            {
                *slot = Some(reply);
            }
            buf.drain(..end);
        }
        if buf.len() > 4096 {
            break;
        }
    }
    // Whatever remains — stray input raced in, or an incomplete reply
    // tail — is real bytes the caller must not lose.
    extra_input.extend_from_slice(&buf);
    if fl >= 0 {
        unsafe {
            libc::fcntl(read_fd, libc::F_SETFL, fl);
        }
    }
    replies
}

/// End index (exclusive) of a complete OSC reply in `buf`, or None if incomplete.
fn osc_reply_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b']' {
            let mut j = i + 2;
            while j < buf.len() {
                if buf[j] == 0x07 {
                    return Some(j + 1);
                }
                if buf[j] == 0x1b && j + 1 < buf.len() && buf[j + 1] == b'\\' {
                    return Some(j + 2);
                }
                j += 1;
            }
            return None;
        }
        i += 1;
    }
    None
}

// --- Byte-level client trace (LRMUX_TRACE) ---
//
// `client-<pid>.trace` records, with millisecond timestamps relative to
// the client's first traced event:
//   KEY — raw bytes read from stdin (before filtering)
//   IN  — bytes that survived the input filter / mouse split (pane-bound)
//   OUT — bytes written to the outer terminal (renderer, status bar, …)
//   EVT — non-byte events (snapshot applied, resize, refresh)

/// Shared client trace file, opened lazily once per process.
fn client_trace_file() -> Option<&'static Mutex<std::fs::File>> {
    static F: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    F.get_or_init(|| {
        crate::log::open_trace(&format!("client-{}.trace", std::process::id())).map(Mutex::new)
    })
    .as_ref()
}

/// Append a timestamped line to the client trace (no-op when disabled).
fn client_trace(tag: &str, data: &[u8]) {
    if let Some(f) = client_trace_file()
        && let Ok(mut g) = f.lock()
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let ms = START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_secs_f64()
            * 1000.0;
        let _ = writeln!(
            g,
            "[+{ms:>11.3}ms] {tag} len={:5} | {}",
            data.len(),
            crate::log::vis_bytes(data)
        );
    }
}

/// Same as `client_trace` for text events (resize, snapshot, …).
fn client_trace_msg(tag: &str, msg: &str) {
    client_trace(tag, msg.as_bytes());
}

/// stdout that tees every write into the client trace file.
/// `write_fmt` (the `write!` macro path) lands as one traced line per
/// call via the `write_all` override.
pub(crate) struct TraceStdout {
    inner: io::Stdout,
}

pub(crate) fn client_stdout() -> TraceStdout {
    TraceStdout {
        inner: io::stdout(),
    }
}

impl Write for TraceStdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        client_trace("OUT", buf);
        self.inner.write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        client_trace("OUT", buf);
        self.inner.write_all(buf)
    }

    // `write!` would otherwise land as one line per format piece —
    // materialize the arguments so a trace line holds a whole sequence.
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        if client_trace_file().is_some() {
            client_trace("OUT", args.to_string().as_bytes());
        }
        self.inner.write_fmt(args)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bar_shows_server_next_to_session() {
        let (text, _) = format_status_bar(
            "lrmux",
            &["zsh".into()],
            0,
            "abc",
            false,
            "infra-284-letsencrypt",
            &[],
            &[],
        );
        let visible = strip_ansi(&text);
        assert!(
            visible.contains("[lrmux]@infra-284-letsencrypt"),
            "{visible}"
        );
    }

    #[test]
    fn status_bar_omits_empty_server() {
        let (text, _) = format_status_bar("lrmux", &["zsh".into()], 0, "abc", false, "", &[], &[]);
        let visible = strip_ansi(&text);
        assert!(!visible.contains('@'), "{visible}");
        assert!(visible.contains("lrmux"), "{visible}");
    }

    #[test]
    fn status_bar_marks_inactive_window_activity() {
        let (text, _) = format_status_bar(
            "lrmux",
            &["zsh".into(), "vim".into()],
            0,
            "abc",
            false,
            "srv",
            &[false, true],
            &[],
        );
        let visible = strip_ansi(&text);
        assert!(visible.contains("●1:vim"), "{visible}");
        assert!(!visible.contains("●0:"), "{visible}");
    }

    #[test]
    fn status_bar_marks_windows_with_mouse_tracking() {
        let (text, _) = format_status_bar(
            "lrmux",
            &["zsh".into(), "vim".into()],
            0,
            "abc",
            false,
            "srv",
            &[],
            &[false, true],
        );
        let visible = strip_ansi(&text);
        assert!(visible.contains("1:vim[M]"), "{visible}");
        assert!(!visible.contains("0:zsh[M]"), "{visible}");
    }

    fn ctl(mode: crate::config::MouseMode) -> MouseCtl {
        MouseCtl {
            mode,
            ovr: MouseOverride::Auto,
            latched: false,
        }
    }

    #[test]
    fn mouse_mask_only_required_follows_the_pane() {
        use crate::config::MouseMode;
        let mut c = ctl(MouseMode::OnlyRequired);
        let mut grid = Grid::new(10, 40, 10);
        assert_eq!(desired_mouse_mask(&mut c, &grid, false), 0);
        grid.mouse_tracking = 0b111;
        assert_eq!(desired_mouse_mask(&mut c, &grid, false), 0b111);
        // Copy mode widens the mask to button events.
        grid.mouse_tracking = 0;
        assert_eq!(desired_mouse_mask(&mut c, &grid, true), 0b010);
    }

    #[test]
    fn mouse_mask_always_on_and_off() {
        use crate::config::MouseMode;
        let grid = Grid::new(10, 40, 10);
        let mut c = ctl(MouseMode::AlwaysOn);
        assert_eq!(desired_mouse_mask(&mut c, &grid, false), 0b010);
        let mut c = ctl(MouseMode::Off);
        assert_eq!(desired_mouse_mask(&mut c, &grid, true), 0);
    }

    #[test]
    fn mouse_mask_auto_enable_latches_after_pane_request() {
        use crate::config::MouseMode;
        let mut c = ctl(MouseMode::AutoEnable);
        let mut grid = Grid::new(10, 40, 10);
        // Off until some pane asks for tracking.
        assert_eq!(desired_mouse_mask(&mut c, &grid, false), 0);
        grid.mouse_tracking = 0b011;
        assert_eq!(desired_mouse_mask(&mut c, &grid, false), 0b011);
        assert!(c.latched);
        // Sticky: stays on (button level) after the pane drops tracking.
        grid.mouse_tracking = 0;
        assert_eq!(desired_mouse_mask(&mut c, &grid, false), 0b010);
    }

    #[test]
    fn mouse_mask_override_wins_both_ways() {
        use crate::config::MouseMode;
        let grid = Grid::new(10, 40, 10);
        let mut c = ctl(MouseMode::Off);
        c.ovr = MouseOverride::ForceOn;
        assert_eq!(desired_mouse_mask(&mut c, &grid, false), 0b010);
        let mut c = ctl(MouseMode::AlwaysOn);
        c.ovr = MouseOverride::ForceOff;
        assert_eq!(desired_mouse_mask(&mut c, &grid, true), 0);
    }

    #[test]
    fn outer_title_format_and_activity_dot() {
        assert_eq!(
            outer_title(
                "srv",
                "work",
                &["zsh".into(), "vim".into()],
                1,
                &[false, false]
            ),
            "lrmux - srv:work:vim"
        );
        // Unseen output on another window prefixes the bullet.
        assert_eq!(
            outer_title(
                "srv",
                "work",
                &["zsh".into(), "vim".into()],
                0,
                &[false, true]
            ),
            "● lrmux - srv:work:zsh"
        );
        // Older servers send no activity flags — plain title.
        assert_eq!(
            outer_title("srv", "work", &["zsh".into()], 0, &[]),
            "lrmux - srv:work:zsh"
        );
        // Control characters in names can't smuggle escapes into the OSC.
        assert_eq!(
            outer_title("s\x1b]8;;x", "we\x07ird", &["z\x1bsh".into()], 0, &[]),
            "lrmux - s]8;;x:weird:zsh"
        );
    }

    #[test]
    fn status_bar_window_spans_cover_each_entry() {
        let (text, spans) = format_status_bar(
            "sess",
            &["zsh".into(), "vim".into()],
            1,
            "abc123",
            false,
            "srv",
            &[true, false],
            &[],
        );
        let visible = strip_ansi(&text);
        assert_eq!(spans.len(), 2);
        // Each span must start exactly on its "i:name" text. The activity
        // bullet is part of the entry's span. find() gives byte offsets —
        // convert to 1-based char columns (● is 3 bytes, 1 column).
        let col_of =
            |needle: &str| visible[..visible.find(needle).unwrap()].chars().count() as u16 + 1;
        let w0 = col_of("●0:zsh");
        let w1 = col_of("1:vim*");
        assert_eq!(spans[0], (w0, w0 + "●0:zsh".chars().count() as u16, 0));
        assert_eq!(spans[1], (w1, w1 + "1:vim*".chars().count() as u16, 1));
        assert_eq!(spans[1].0, spans[0].1 + 3); // joined with " | "
        assert!(visible.contains("●0:zsh | 1:vim*"), "{visible}");
    }

    fn socketpair() -> (i32, i32) {
        let mut fds = [0i32; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            0
        );
        (fds[0], fds[1])
    }

    /// Terminal-side helper: read the probe query, then write `response`.
    fn fake_terminal(fd: i32, response: &'static [u8]) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut q = [0u8; 128];
            unsafe {
                libc::read(fd, q.as_mut_ptr() as *mut _, q.len());
                libc::write(fd, response.as_ptr() as *const _, response.len());
            }
        })
    }

    #[test]
    fn osc_probe_collects_both_replies_in_one_roundtrip() {
        let (a, b) = socketpair();
        let term = fake_terminal(
            b,
            b"\x1b]10;rgb:aaaa/bbbb/cccc\x07\x1b]11;rgb:1111/2222/3333\x07",
        );
        let mut extra = Vec::new();
        let res = query_osc_colors_on_fds(a, a, &[10, 11], true, &mut extra);
        unsafe {
            libc::close(a);
        }
        term.join().unwrap();
        assert_eq!(
            res[0].as_deref(),
            Some(b"\x1b]10;rgb:aaaa/bbbb/cccc\x07".as_slice())
        );
        assert_eq!(
            res[1].as_deref(),
            Some(b"\x1b]11;rgb:1111/2222/3333\x07".as_slice())
        );
        assert!(extra.is_empty());
    }

    #[test]
    fn osc_probe_returns_stray_keystrokes_as_input() {
        let (a, b) = socketpair();
        // 'x' arrives before the reply, 'y' after — both are user input.
        let term = fake_terminal(b, b"x\x1b]10;rgb:aaaa/bbbb/cccc\x07y");
        let mut extra = Vec::new();
        let res = query_osc_colors_on_fds(a, a, &[10], true, &mut extra);
        unsafe {
            libc::close(a);
        }
        term.join().unwrap();
        assert!(res[0].is_some());
        assert_eq!(extra, b"xy");
    }

    #[test]
    fn osc_probe_times_out_fast_on_silent_terminal() {
        let (a, b) = socketpair();
        let term = fake_terminal(b, b"hi"); // input, but no reply
        let mut extra = Vec::new();
        let t = std::time::Instant::now();
        let res = query_osc_colors_on_fds(a, a, &[10, 11], true, &mut extra);
        unsafe {
            libc::close(a);
        }
        term.join().unwrap();
        assert!(res.iter().all(Option::is_none));
        assert_eq!(extra, b"hi");
        // Must stay well under the old 2s-per-attempt behavior.
        assert!(t.elapsed() < std::time::Duration::from_millis(900));
    }

    #[test]
    fn osc_probe_reports_partial_replies() {
        let (a, b) = socketpair();
        let term = fake_terminal(b, b"\x1b]11;rgb:1111/2222/3333\x07");
        let mut extra = Vec::new();
        let res = query_osc_colors_on_fds(a, a, &[10, 11], true, &mut extra);
        unsafe {
            libc::close(a);
        }
        term.join().unwrap();
        assert!(res[0].is_none());
        assert!(res[1].is_some());
    }
}
