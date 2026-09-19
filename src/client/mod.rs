// Client process: raw mode, input relay, prefix detection, output render.

pub mod control;
pub mod copy_mode;
pub mod render;
pub mod selector;
pub mod terminal;

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::grid::{Cell, Grid};
use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};
use crate::version;

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
) -> io::Result<()> {
    // Connect to the server.
    let mut stream = ipc::connect(socket_path)?;
    let stream_fd = stream.as_raw_fd();

    // Enter raw mode on the controlling terminal.
    let _raw_guard = terminal::enter_raw_mode()?;

    // Enter raw mode. Do NOT use alternate screen — we want the terminal's
    // native scrollback to capture content that scrolls off the top.
    // A scroll region is set (excluding the status bar) so that \x1b[S
    // scrolls only the content area, pushing lines into the terminal's
    // scrollback buffer.
    {
        let mut stdout = io::stdout();
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

    // Probe outer TTY defaults (OSC 10/11) so the server can paint HTML
    // captures without waiting for vim to ask. Raw mode is already on —
    // replies won't echo onto the screen.
    report_outer_term_palette(&mut stream)?;

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
        let mut stdout = io::stdout();
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
            // Update scroll region to exclude the status bar.
            {
                let view_rows = term_rows.saturating_sub(1).max(1);
                let mut stdout = io::stdout();
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
                let mut stdout = io::stdout();
                cm.render(&mut stdout, &grid, view_rows, term_cols, &status_text)?;
            } else {
                // Clear screen and re-render everything.
                let mut stdout = io::stdout();
                stdout.write_all(b"\x1b[2J\x1b[H")?;
                renderer.invalidate();
                grid.mark_all_dirty();
                renderer.render(&mut stdout, &mut grid)?;
                render_filler(&mut stdout, grid.rows(), grid.cols(), term_rows, term_cols)?;
                render_status_bar(&mut stdout, &status_text, term_rows, term_cols, &grid)?;
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

        // Use a 500ms poll timeout so we can expire flash messages.
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, 500) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }

        // Check if flash message expired.
        if let Some(deadline) = flash_deadline
            && std::time::Instant::now() >= deadline
        {
            flash_msg = None;
            flash_deadline = None;
            // Re-render the normal status bar.
            let mut stdout = io::stdout();
            render_status_bar(&mut stdout, &status_text, term_rows, term_cols, &grid)?;
        }

        // If we have a flash message, render it on the status bar.
        if let Some(ref msg) = flash_msg {
            let mut stdout = io::stdout();
            render_flash_status_bar(&mut stdout, msg, &status_text, term_rows, term_cols, &grid)?;
        }

        // stdin → prefix detection → server (as PaneInput or commands)
        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 8192];
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let input = &buf[..n as usize];

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
                                    let mut stdout = io::stdout();
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
                                    let mut stdout = io::stdout();
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
                                    let mut stdout = io::stdout();
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
                                )?;
                            }
                            copy_mode::CopyAction::Continue => {}
                        }
                    }
                } else if matches!(confirm_state, ConfirmState::None) {
                    let (
                        passthrough,
                        detach,
                        confirm,
                        remaining,
                        enter_copy_mode,
                        paste,
                        flash,
                        show_help,
                        request_session_chooser,
                    ) = process_prefix(
                        input,
                        &mut prefix_state,
                        &mut stream,
                        current_window_count,
                        session_count,
                        last_window,
                    )?;
                    if show_help {
                        show_help_overlay(&server_version);
                        // Invalidate the renderer so the screen is fully redrawn.
                        renderer.invalidate();
                        // Re-establish the scroll region and re-render.
                        let view_rows = term_rows.saturating_sub(1);
                        let mut stdout = io::stdout();
                        write!(stdout, "\x1b[1;{}r", view_rows.max(1)).ok();
                        renderer.render(&mut stdout, &mut grid)?;
                    }
                    if request_session_chooser {
                        pending_session_chooser = true;
                    }
                    if let Some(msg) = flash {
                        flash_msg = Some(msg);
                        flash_deadline =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                        let mut stdout = io::stdout();
                        render_flash_status_bar(
                            &mut stdout,
                            flash_msg.as_ref().unwrap(),
                            &status_text,
                            term_rows,
                            term_cols,
                            &grid,
                        )?;
                    }
                    if !passthrough.is_empty() {
                        let msg = proto::encode_client(&ClientMsg::PaneInput { data: passthrough });
                        proto::send(&mut stream, &msg)?;
                    }
                    if detach {
                        let msg = proto::encode_client(&ClientMsg::Detach);
                        proto::send(&mut stream, &msg)?;
                        exit_reason = Some("detached".to_string());
                        break;
                    }
                    if enter_copy_mode {
                        copy_mode = Some(copy_mode::CopyMode::new(
                            grid.scrollback.len(),
                            grid.cursor_row,
                            grid.cursor_col,
                        ));
                        let view_rows = term_rows.saturating_sub(1);
                        let mut stdout = io::stdout();
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
                                    let mut stdout = io::stdout();
                                    render_status_bar(
                                        &mut stdout,
                                        &status_text,
                                        term_rows,
                                        term_cols,
                                        &grid,
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
                            let mut stdout = io::stdout();
                            render_status_bar(
                                &mut stdout,
                                &status_text,
                                term_rows,
                                term_cols,
                                &grid,
                            )?;
                        }
                        ConfirmAction::Cancelled => {
                            confirm_state = ConfirmState::None;
                            let mut stdout = io::stdout();
                            render_status_bar(
                                &mut stdout,
                                &status_text,
                                term_rows,
                                term_cols,
                                &grid,
                            )?;
                        }
                        ConfirmAction::Continue => {
                            render_confirm_prompt(&confirm_state, term_rows);
                        }
                    }
                }
            } else if n == 0 {
                break;
            }
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
                let n = unsafe { libc::read(stream_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
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
                            for row in rows {
                                grid.scrollback.push(row);
                            }
                            // Scroll the terminal to push content into the
                            // terminal's native scrollback buffer — but only
                            // for live scroll. Replay chunks (history after a
                            // GridSnapshot) would scroll the just-rendered
                            // content off screen, leaving it blank.
                            if n > 0 && copy_mode.is_none() && !replay {
                                let mut stdout = io::stdout();
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
                        } => {
                            for (row, cells) in &dirty {
                                apply_row(&mut grid, *row as usize, cells);
                            }
                            grid.cursor_row = cursor_row as usize;
                            grid.cursor_col = cursor_col as usize;
                            grid.cursor_visible = cursor_visible;
                            needs_render = true;
                        }
                        ServerMsg::GridSnapshot {
                            rows,
                            cols,
                            cells,
                            cursor_row,
                            cursor_col,
                            cursor_visible,
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
                            let mut stdout = io::stdout();
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
                            )?;
                        }
                        ServerMsg::StatusBarUpdate {
                            session,
                            windows,
                            active,
                            session_count: sc,
                            high_output: h,
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
                            status_text = format_status_bar(
                                &session,
                                &windows,
                                active as usize,
                                &server_version,
                                h,
                            );
                            let mut stdout = io::stdout();
                            if let Some(ref msg) = flash_msg {
                                render_flash_status_bar(
                                    &mut stdout,
                                    msg,
                                    &status_text,
                                    term_rows,
                                    term_cols,
                                    &grid,
                                )?;
                            } else {
                                render_status_bar(
                                    &mut stdout,
                                    &status_text,
                                    term_rows,
                                    term_cols,
                                    &grid,
                                )?;
                            }
                        }
                        ServerMsg::PaneExit { .. } => {
                            exit_reason = Some("session ended (last pane exited)".to_string());
                            break;
                        }
                        ServerMsg::IdentifyAck { .. } => {}
                        ServerMsg::SessionList {
                            sessions,
                            address: _,
                        } => {
                            if pending_session_chooser {
                                pending_session_chooser = false;
                                if let Some(name) = show_session_chooser(&sessions) {
                                    let msg = proto::encode_client(&ClientMsg::SelectSession {
                                        name: name.clone(),
                                    });
                                    let _ = proto::send(&mut stream, &msg);
                                }
                                // Invalidate the renderer so the screen is fully redrawn.
                                renderer.invalidate();
                                let view_rows = term_rows.saturating_sub(1);
                                let mut stdout = io::stdout();
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
                            let mut stdout = io::stdout();
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
                            let _ = io::stdout().flush();
                            if let Some(reply) = query_outer_osc_color(code, bell_terminated) {
                                let msg = proto::encode_client(&ClientMsg::TermOscReply {
                                    pane_id,
                                    data: reply,
                                });
                                proto::send(&mut stream, &msg)?;
                            }
                        }
                    }
                }
                // Render once per socket batch instead of once per frame —
                // during a burst many GridUpdates arrive in a single read.
                if needs_render && exit_reason.is_none() {
                    if let Some(ref cm) = copy_mode {
                        let view_rows = term_rows.saturating_sub(1);
                        let mut stdout = io::stdout();
                        cm.render(&mut stdout, &grid, view_rows, term_cols, &status_text)?;
                    } else {
                        let mut stdout = io::stdout();
                        renderer.render(&mut stdout, &mut grid)?;
                        render_status_bar(&mut stdout, &status_text, term_rows, term_cols, &grid)?;
                    }
                }
            } else if last == 0 {
                // Server closed the connection (EOF).
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
    Ok(())
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
/// Returns (passthrough, detach, confirm, remaining, enter_copy_mode, paste).
/// When a confirm dialog is triggered, remaining bytes after the trigger are returned
/// so the caller can process them with process_confirm.
#[allow(clippy::type_complexity)]
fn process_prefix(
    input: &[u8],
    state: &mut PrefixState,
    stream: &mut std::os::unix::net::UnixStream,
    window_count: usize,
    session_count: usize,
    last_window: Option<u8>,
) -> io::Result<(
    Vec<u8>,
    bool,
    Option<ConfirmState>,
    Vec<u8>,
    bool,
    bool,
    Option<String>,
    bool,
    bool,
)> {
    let mut passthrough: Vec<u8> = Vec::new();
    let mut detach = false;
    let mut confirm: Option<ConfirmState> = None;
    let mut enter_copy_mode = false;
    let mut paste = false;
    let mut flash: Option<String> = None;
    let mut show_help = false;
    let mut request_session_chooser = false;

    for (i, &byte) in input.iter().enumerate() {
        if confirm.is_some() {
            // Stop processing — return remaining bytes for the confirm handler.
            return Ok((
                passthrough,
                detach,
                confirm,
                input[i..].to_vec(),
                enter_copy_mode,
                paste,
                flash,
                show_help,
                request_session_chooser,
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
        confirm,
        Vec::new(),
        enter_copy_mode,
        paste,
        flash,
        show_help,
        request_session_chooser,
    ))
}

/// Send a command message to the server.
fn send_cmd(stream: &mut std::os::unix::net::UnixStream, msg: &ClientMsg) -> io::Result<()> {
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
    stream: &mut std::os::unix::net::UnixStream,
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
    }
}

/// Render the confirmation prompt on the status bar line.
fn render_confirm_prompt(state: &ConfirmState, term_rows: usize) {
    let mut stdout = io::stdout();
    let row = term_rows;
    // Clear the line and write the prompt.
    write!(stdout, "\x1b[{};1H\x1b[2K", row).ok();
    match state {
        ConfirmState::None => {}
        ConfirmState::KillWindow => {
            write!(stdout, "\x1b[43;30m Kill current window? (y/n) \x1b[0m").ok();
        }
        ConfirmState::KillSession {
            input,
            target,
            window_count,
        } => {
            write!(
                stdout,
                "\x1b[41;97m Kill session '{}' ({} window{})? Type the name to confirm: {}\x1b[0m",
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
                "\x1b[44;97m Rename session: {}\x1b[1;93m_\x1b[0m\x1b[44;97m  (Enter=confirm, Esc=cancel)\x1b[0m",
                input
            )
            .ok();
        }
    }
    stdout.flush().ok();
}

/// Format the status bar text with colors.
/// The bar uses a blue background; the active window is highlighted in bold yellow.
/// The session name is shown first, then the window list.
fn format_status_bar(
    session: &str,
    windows: &[String],
    active: usize,
    server_version: &str,
    high_output: bool,
) -> String {
    // Blue background + white text for inactive windows.
    const BAR: &str = "\x1b[44;97m"; // bg blue, bright white
    // Active window: bold bright yellow on blue.
    const ACTIVE: &str = "\x1b[1;44;93m"; // bold, bg blue, bright yellow
    // Session name: bold bright cyan on blue.
    const SESSION: &str = "\x1b[1;44;96m"; // bold, bg blue, bright cyan
    const RESET: &str = "\x1b[0m";
    const WARN: &str = "\x1b[1;44;31m"; // bold red on blue

    let server_hash = server_version.rsplit('-').next().unwrap_or(server_version);
    let mismatch = server_version != version::VERSION;
    let version_marker = if mismatch {
        format!("{WARN}!{RESET}")
    } else {
        String::new()
    };
    let burst_marker = if high_output {
        format!("{WARN}[BURST]{RESET} ")
    } else {
        String::new()
    };

    let mut parts: Vec<String> = Vec::new();
    for (i, name) in windows.iter().enumerate() {
        if i == active {
            parts.push(format!("{}{}:{}*{}{}", ACTIVE, i, name, BAR, burst_marker));
        } else {
            parts.push(format!("{}:{}", i, name));
        }
    }
    format!(
        "{}lrmux {}{}{} | {}{} | {}{}",
        BAR,
        version_marker,
        server_hash,
        BAR,
        SESSION,
        session,
        parts.join("  "),
        RESET
    )
}

/// Render the status bar at the bottom of the terminal.
/// The status bar always occupies the last row of the terminal (term_rows).
/// After rendering, the cursor is repositioned to the grid cursor location.
fn render_status_bar(
    stdout: &mut io::Stdout,
    text: &str,
    term_rows: usize,
    term_cols: usize,
    grid: &Grid,
) -> io::Result<()> {
    // Position cursor at the last row of the terminal (1-based).
    let row = term_rows;
    // Clear the line first, then write the colored status bar.
    write!(stdout, "\x1b[{};1H\x1b[2K", row)?;
    // Write the full status bar text (it includes its own ANSI colors).
    stdout.write_all(text.as_bytes())?;
    // Measure visible width (excluding ANSI escape sequences) and pad
    // the rest of the line with the bar background color so the
    // blue background extends to the right edge of the terminal.
    let max_cols = term_cols.min(500);
    let visible_len = strip_ansi(text).chars().count();
    if visible_len < max_cols {
        // Use the same blue background, no text attributes.
        write!(stdout, "\x1b[44m{}", " ".repeat(max_cols - visible_len))?;
    }
    // Reset attributes.
    stdout.write_all(b"\x1b[0m")?;
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
    stdout: &mut io::Stdout,
    msg: &str,
    normal_text: &str,
    term_rows: usize,
    term_cols: usize,
    grid: &Grid,
) -> io::Result<()> {
    let row = term_rows;
    write!(stdout, "\x1b[{};1H\x1b[2K", row)?;
    let max_cols = term_cols.min(500);

    // Strip escape sequences from normal_text to measure visible width.
    let normal_visible: String = strip_ansi(normal_text);
    let normal_len = normal_visible.chars().count();

    // Flash message in bold yellow on blue, with a separator.
    const FLASH: &str = "\x1b[1;44;93m"; // bold, bg blue, bright yellow
    const BAR: &str = "\x1b[44;97m"; // bg blue, bright white
    const RESET: &str = "\x1b[0m";

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
        write!(stdout, "\x1b[44m{}", " ".repeat(max_cols - total_visible))?;
    }
    stdout.write_all(b"\x1b[0m")?;

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
) -> io::Result<()> {
    let mut stdout = io::stdout();
    // Hide cursor (copy mode shows it; normal mode hides it).
    stdout.write_all(b"\x1b[?25l")?;
    // Reset scroll region to exclude status bar, then clear and re-render.
    let view_rows = term_rows.saturating_sub(1).max(1);
    write!(stdout, "\x1b[1;{}r\x1b[2J\x1b[H", view_rows)?;
    renderer.invalidate();
    grid.mark_all_dirty();
    renderer.render(&mut stdout, grid)?;
    render_filler(&mut stdout, grid.rows(), grid.cols(), term_rows, term_cols)?;
    render_status_bar(&mut io::stdout(), status_text, term_rows, term_cols, grid)?;
    Ok(())
}

/// Render the filler region: the area beyond the canonical grid when the
/// terminal is larger than the grid. Uses a dim background with a thin
/// border line separating content from filler (per §2.16 of the design doc).
fn render_filler(
    stdout: &mut io::Stdout,
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
    let mut stdout = io::stdout();
    // Reset scroll region to full screen, show cursor, clear screen.
    let _ = stdout.write_all(b"\x1b[r\x1b[?25h\x1b[2J\x1b[H");
    let _ = stdout.flush();
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
    let mut stdout = io::stdout();
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
    let mut stdout = io::stdout();
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
    let mut stdout = io::stdout();
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
        ("Ctrl-A d / Ctrl-D", "Detach"),
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
pub(crate) fn report_outer_term_palette(stream: &mut impl Write) -> io::Result<()> {
    let fg = query_outer_osc_color(10, true)
        .and_then(|d| crate::term::parse_osc_color_reply(&d))
        .map(|(_, rgb)| rgb);
    let bg = query_outer_osc_color(11, true)
        .and_then(|d| crate::term::parse_osc_color_reply(&d))
        .map(|(_, rgb)| rgb);
    if fg.is_none() && bg.is_none() {
        return Ok(());
    }
    let msg = proto::encode_client(&ClientMsg::TermPalette { fg, bg });
    proto::send(stream, &msg)
}

/// Query the outer terminal's OSC 10 (fg) or 11 (bg) color.
///
/// Tries stdout/stdin first (the fds the interactive client already has on
/// the user's terminal), then `/dev/tty` as a fallback. A failed query
/// surfaces as vim E1568 and wrong `background` / colorscheme colors.
pub(crate) fn query_outer_osc_color(code: u8, bell_terminated: bool) -> Option<Vec<u8>> {
    if code != 10 && code != 11 {
        return None;
    }

    let mut query = Vec::with_capacity(16);
    query.extend_from_slice(b"\x1b]");
    query.extend_from_slice(code.to_string().as_bytes());
    query.extend_from_slice(b";?");
    if bell_terminated {
        query.push(0x07);
    } else {
        query.extend_from_slice(b"\x1b\\");
    }

    // Prefer the fds we already own; fall back to /dev/tty (needed for -CC
    // where stdout is the control-mode channel, not a raw terminal).
    if let Some(reply) = query_osc_on_fds(libc::STDIN_FILENO, libc::STDOUT_FILENO, &query) {
        return Some(reply);
    }
    query_osc_via_dev_tty(&query)
}

/// Control-mode variant: never write OSC to stdout (that's the tmux control
/// channel). Only query via `/dev/tty`.
pub(crate) fn query_outer_osc_color_for_control(
    code: u8,
    bell_terminated: bool,
) -> Option<Vec<u8>> {
    if code != 10 && code != 11 {
        return None;
    }
    let mut query = Vec::with_capacity(16);
    query.extend_from_slice(b"\x1b]");
    query.extend_from_slice(code.to_string().as_bytes());
    query.extend_from_slice(b";?");
    if bell_terminated {
        query.push(0x07);
    } else {
        query.extend_from_slice(b"\x1b\\");
    }
    query_osc_via_dev_tty(&query)
}

fn query_osc_via_dev_tty(query: &[u8]) -> Option<Vec<u8>> {
    let fd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    if fd < 0 {
        return None;
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
    query_osc_on_fds(tty.0, tty.0, query)
}

fn query_osc_on_fds(read_fd: i32, write_fd: i32, query: &[u8]) -> Option<Vec<u8>> {
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

    // Drain pending input so leftover key bytes aren't mistaken for the reply.
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
                continue;
            }
            break;
        }
    }

    let w = unsafe { libc::write(write_fd, query.as_ptr() as *const _, query.len()) };
    if w < 0 {
        if fl >= 0 {
            unsafe {
                libc::fcntl(read_fd, libc::F_SETFL, fl);
            }
        }
        return None;
    }
    if write_fd == libc::STDOUT_FILENO {
        let _ = io::stdout().flush();
    }

    let mut buf = Vec::with_capacity(128);
    let mut tmp = [0u8; 256];
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    if fl >= 0 {
        unsafe {
            libc::fcntl(read_fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
    let result = loop {
        if std::time::Instant::now() >= deadline {
            break None;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let mut pfd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().min(250) as i32;
        let pret = unsafe { libc::poll(&mut pfd, 1, ms) };
        if pret <= 0 {
            continue;
        }
        let n = unsafe { libc::read(read_fd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
        if n <= 0 {
            continue;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
        if let Some(end) = osc_reply_end(&buf) {
            if let Some(start) = buf.windows(2).position(|w| w == b"\x1b]") {
                break Some(buf[start..end].to_vec());
            }
            break Some(buf[..end].to_vec());
        }
        if buf.len() > 4096 {
            break None;
        }
    };
    if fl >= 0 {
        unsafe {
            libc::fcntl(read_fd, libc::F_SETFL, fl);
        }
    }
    result
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
