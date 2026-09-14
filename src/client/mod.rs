// Client process: raw mode, input relay, prefix detection, output render.

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
    let identify = proto::encode_client(&ClientMsg::Identify { rows, cols });
    proto::send(&mut stream, &identify)?;

    // Wait for IdentifyAck to get grid dimensions.
    let (grid_rows, grid_cols) = match proto::decode_server(&mut stream) {
        Ok(ServerMsg::IdentifyAck { rows, cols }) => (rows as usize, cols as usize),
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
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        let msg = proto::encode_client(&ClientMsg::NewSession { name, cwd });
        proto::send(&mut stream, &msg)?;
    }
    // If requested, switch to an existing session by name.
    if let Some(ref name) = select_session {
        let msg = proto::encode_client(&ClientMsg::SelectSession { name: name.clone() });
        proto::send(&mut stream, &msg)?;
    }

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
    // When true, skip \x1b[NS terminal scroll for the next ScrollbackUpdate.
    // Set by GridSnapshot (window/session switch) because the scrollback
    // replayed there is history, not new scroll-off — emitting \x1b[NS
    // would scroll the just-rendered content off the screen.
    let mut skip_terminal_scroll = false;

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
                        show_help_overlay();
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
            let mut buf = [0u8; 8192];
            let n = unsafe { libc::read(stream_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                server_buf.extend_from_slice(&buf[..n as usize]);
                while let Some(msg) = try_parse_server_frame(&mut server_buf)? {
                    match msg {
                        ServerMsg::ScrollbackUpdate { rows } => {
                            // Push to internal scrollback (for copy mode).
                            let n = rows.len();
                            for row in rows {
                                grid.scrollback.push(row);
                            }
                            // Scroll the terminal to push content into the
                            // terminal's native scrollback buffer.
                            // Skip in copy mode (terminal is showing copy view).
                            // Also skip when this scrollback is a replay of the
                            // window's history (after GridSnapshot) — emitting
                            // \x1b[NS here would scroll the just-rendered content
                            // off the screen, leaving it blank.
                            if n > 0 && copy_mode.is_none() && !skip_terminal_scroll {
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
                            // Reset the flag — only the first ScrollbackUpdate
                            // after a GridSnapshot should be skipped.
                            skip_terminal_scroll = false;
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
                            if let Some(ref cm) = copy_mode {
                                // In copy mode: re-render the copy mode view.
                                let view_rows = term_rows.saturating_sub(1);
                                let mut stdout = io::stdout();
                                cm.render(&mut stdout, &grid, view_rows, term_cols, &status_text)?;
                            } else {
                                let mut stdout = io::stdout();
                                renderer.render(&mut stdout, &mut grid)?;
                                render_status_bar(
                                    &mut stdout,
                                    &status_text,
                                    term_rows,
                                    term_cols,
                                    &grid,
                                )?;
                            }
                        }
                        ServerMsg::GridSnapshot {
                            rows,
                            cols,
                            cells,
                            cursor_row,
                            cursor_col,
                            cursor_visible,
                        } => {
                            // The server sends ScrollbackUpdate right after
                            // GridSnapshot to replay the window's history.
                            // We must NOT emit \x1b[NS for that replay — it
                            // would scroll the just-rendered content off screen.
                            skip_terminal_scroll = true;
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
                            status_text = format_status_bar(&session, &windows, active as usize);
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
                            break;
                        }
                        ServerMsg::IdentifyAck { .. } => {}
                        ServerMsg::SessionList { sessions } => {
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
                            eprintln!("\r\nlrmux: server error: {msg}\r");
                            break;
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

        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
    }

    restore_terminal();
    Ok(())
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
fn format_status_bar(session: &str, windows: &[String], active: usize) -> String {
    // Blue background + white text for inactive windows.
    const BAR: &str = "\x1b[44;97m"; // bg blue, bright white
    // Active window: bold bright yellow on blue.
    const ACTIVE: &str = "\x1b[1;44;93m"; // bold, bg blue, bright yellow
    // Session name: bold bright cyan on blue.
    const SESSION: &str = "\x1b[1;44;96m"; // bold, bg blue, bright cyan
    const RESET: &str = "\x1b[0m";

    let mut parts: Vec<String> = Vec::new();
    for (i, name) in windows.iter().enumerate() {
        if i == active {
            parts.push(format!("{}{}:{}*{}", ACTIVE, i, name, BAR));
        } else {
            parts.push(format!("{}:{}", i, name));
        }
    }
    format!(
        "{}lrmux | {}{} | {}{}",
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
fn show_help_overlay() {
    use std::io::Read;
    let mut stdout = io::stdout();
    let (rows, _cols) = terminal::get_size();

    write!(stdout, "\x1b[2J\x1b[H\x1b[?25h").ok();
    stdout.flush().ok();

    let title = "lrmux keybindings (press any key to dismiss)";
    write!(stdout, "\x1b[1;36m{title}\x1b[0m\r\n").ok();
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
