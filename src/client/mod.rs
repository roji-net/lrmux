// Client process: raw mode, input relay, prefix detection, output render.

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

    // Enter alternate screen.
    {
        let mut stdout = io::stdout();
        stdout.write_all(b"\x1b[?1049h")?;
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

    // Status bar text (updated by the server).
    let mut status_text = String::new();
    // Current session name (from StatusBarUpdate, used for kill-session confirmation).
    let mut current_session = String::new();
    // Number of windows in the current session (from StatusBarUpdate).
    let mut current_window_count: usize = 0;

    // If requested, create a new session on the server right after handshake.
    if let Some(name) = new_session {
        let msg = proto::encode_client(&ClientMsg::NewSession { name });
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

    // Install SIGWINCH handler so terminal resizes are detected.
    install_winch_handler();

    loop {
        // Check if the terminal was resized.
        if WINCH.swap(false, Ordering::Relaxed) {
            let (rows, cols) = terminal::get_size();
            let msg = proto::encode_client(&ClientMsg::Resize { rows, cols });
            let _ = proto::send(&mut stream, &msg);
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

        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }

        // stdin → prefix detection → server (as PaneInput or commands)
        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 8192];
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let input = &buf[..n as usize];
                if matches!(confirm_state, ConfirmState::None) {
                    let (passthrough, detach, confirm, remaining) =
                        process_prefix(input, &mut prefix_state, &mut stream)?;
                    if !passthrough.is_empty() {
                        let msg = proto::encode_client(&ClientMsg::PaneInput { data: passthrough });
                        proto::send(&mut stream, &msg)?;
                    }
                    if detach {
                        let msg = proto::encode_client(&ClientMsg::Detach);
                        proto::send(&mut stream, &msg)?;
                        break;
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
                        confirm_state = c;
                        render_confirm_prompt(&confirm_state, grid.rows());
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
                                        grid.rows(),
                                        &grid,
                                    )?;
                                }
                                ConfirmAction::Continue => {
                                    render_confirm_prompt(&confirm_state, grid.rows());
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
                            render_status_bar(&mut stdout, &status_text, grid.rows(), &grid)?;
                        }
                        ConfirmAction::Cancelled => {
                            confirm_state = ConfirmState::None;
                            let mut stdout = io::stdout();
                            render_status_bar(&mut stdout, &status_text, grid.rows(), &grid)?;
                        }
                        ConfirmAction::Continue => {
                            render_confirm_prompt(&confirm_state, grid.rows());
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
                            let mut stdout = io::stdout();
                            renderer.render(&mut stdout, &mut grid)?;
                            render_status_bar(&mut stdout, &status_text, grid.rows(), &grid)?;
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
                            stdout.write_all(b"\x1b[2J\x1b[H")?;
                            renderer.render(&mut stdout, &mut grid)?;
                            render_status_bar(&mut stdout, &status_text, grid.rows(), &grid)?;
                        }
                        ServerMsg::StatusBarUpdate {
                            session,
                            windows,
                            active,
                        } => {
                            current_session = session.clone();
                            current_window_count = windows.len();
                            status_text = format_status_bar(&session, &windows, active as usize);
                            let mut stdout = io::stdout();
                            render_status_bar(&mut stdout, &status_text, grid.rows(), &grid)?;
                        }
                        ServerMsg::PaneExit { .. } => {
                            break;
                        }
                        ServerMsg::IdentifyAck { .. } => {}
                        ServerMsg::SessionList { .. } => {}
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
/// Returns (passthrough bytes, should_detach, optional confirm dialog, remaining unprocessed bytes).
/// When a confirm dialog is triggered, remaining bytes after the trigger are returned
/// so the caller can process them with process_confirm.
#[allow(clippy::type_complexity)]
fn process_prefix(
    input: &[u8],
    state: &mut PrefixState,
    stream: &mut std::os::unix::net::UnixStream,
) -> io::Result<(Vec<u8>, bool, Option<ConfirmState>, Vec<u8>)> {
    let mut passthrough: Vec<u8> = Vec::new();
    let mut detach = false;
    let mut confirm: Option<ConfirmState> = None;

    for (i, &byte) in input.iter().enumerate() {
        if confirm.is_some() {
            // Stop processing — return remaining bytes for the confirm handler.
            return Ok((passthrough, detach, confirm, input[i..].to_vec()));
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
                    // Double prefix → send literal prefix to child.
                    PREFIX => {
                        passthrough.push(PREFIX);
                    }
                    // 'c' → new window.
                    b'c' => {
                        send_cmd(stream, &ClientMsg::NewWindow)?;
                    }
                    // 'n' or Space → next window.
                    b'n' | b' ' => {
                        send_cmd(stream, &ClientMsg::NextWindow)?;
                    }
                    // 'p' → previous window.
                    b'p' => {
                        send_cmd(stream, &ClientMsg::PrevWindow)?;
                    }
                    // 'd' → detach (handled by caller after passthrough is sent).
                    b'd' => {
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
                    // 'C' → new session (uppercase, like lowercase 'c' for new window).
                    b'C' => {
                        send_cmd(stream, &ClientMsg::NewSession { name: None })?;
                    }
                    // 'N' → next session.
                    b'N' => {
                        send_cmd(stream, &ClientMsg::NextSession)?;
                    }
                    // 'P' → previous session.
                    b'P' => {
                        send_cmd(stream, &ClientMsg::PrevSession)?;
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

    Ok((passthrough, detach, confirm, Vec::new()))
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
    }
}

/// Render the confirmation prompt on the status bar line.
fn render_confirm_prompt(state: &ConfirmState, grid_rows: usize) {
    let mut stdout = io::stdout();
    let row = grid_rows + 1;
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

/// Render the status bar at the bottom of the screen.
/// The status bar occupies the row immediately after the grid.
/// After rendering, the cursor is repositioned to the grid cursor location.
fn render_status_bar(
    stdout: &mut io::Stdout,
    text: &str,
    grid_rows: usize,
    grid: &Grid,
) -> io::Result<()> {
    // Position cursor at the row after the grid (1-based).
    let row = grid_rows + 1;
    // Clear the line first, then write the colored status bar.
    write!(stdout, "\x1b[{};1H\x1b[2K", row)?;
    // Truncate text to terminal width (use grid cols as approximation).
    let max_cols = 200; // generous upper bound; terminal will clip
    let display: String = text.chars().take(max_cols).collect();
    stdout.write_all(display.as_bytes())?;
    // Reset attributes.
    stdout.write_all(b"\x1b[0m")?;
    // Reposition cursor to the grid cursor location so the user sees
    // the cursor in the pane, not on the status bar.
    if grid.cursor_visible {
        write!(
            stdout,
            "\x1b[{};{}H",
            grid.cursor_row + 1,
            grid.cursor_col + 1
        )?;
    }
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
    let _ = stdout.write_all(b"\x1b[?25h\x1b[?1049l");
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
