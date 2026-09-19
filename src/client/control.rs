// Control mode client (tmux -CC compatible).
//
// Connects to the server in control mode, emits the DCS startup sequence,
// relays notifications to stdout, and reads tmux-style commands from stdin.
//
// CRITICAL: never block on a single fd. Stdin reads grab whatever bytes are
// available and buffer partial lines; stdout and server-socket writes are
// non-blocking with pending buffers drained on POLLOUT. A blocked write
// (e.g. iTerm2 showing a modal dialog and not draining the PTY) must not
// starve other fds — otherwise the server blocks on send() and freezes
// every client.

use std::collections::VecDeque;
use std::io::{self, Write};

use std::os::fd::AsRawFd;

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};
use crate::term::ControlModeGuard;

/// Debug dumper for the control-mode byte stream. Enabled via the
/// LRMUX_CC_DEBUG env var: set it to a path to log to, or any non-empty
/// value to use /tmp/lrmux-cc-debug.log. Everything the client reads from
/// stdin, sends to the server, receives from the server, and writes to
/// stdout is logged with escaped non-printable bytes — enough to diff
/// against `tmux -CC -vv` logs byte-for-byte.
struct CcDebug {
    file: Option<std::fs::File>,
}

impl CcDebug {
    fn open() -> Self {
        let file = std::env::var_os("LRMUX_CC_DEBUG").and_then(|v| {
            let path = if v == "1" || v.is_empty() {
                std::ffi::OsString::from("/tmp/lrmux-cc-debug.log")
            } else {
                v
            };
            std::fs::File::create(path).ok()
        });
        Self { file }
    }

    fn log(&mut self, tag: &str, data: &[u8]) {
        if let Some(f) = &mut self.file {
            let mut line = String::with_capacity(data.len() + 16);
            line.push_str(tag);
            line.push(' ');
            for &b in data {
                if (0x20..0x7f).contains(&b) {
                    line.push(b as char);
                } else {
                    line.push_str(&format!("\\x{b:02x}"));
                }
            }
            line.push('\n');
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

/// Run the control mode client.
///
/// 1. Connect to the server.
/// 2. Send IdentifyControl.
/// 3. Emit the DCS startup sequence (`ESC P1000p`).
/// 4. Read notifications from the server and print them to stdout.
/// 5. Read tmux-style commands from stdin and send them to the server.
/// 6. On %exit or server disconnect, emit `%exit` and exit.
pub fn run(socket_path: &std::path::Path) -> io::Result<()> {
    let mut stream = ipc::connect(socket_path)?;

    // Get terminal size (default to 24x80 if not available).
    let (rows, cols) = crate::client::terminal::get_size();

    // Enter control mode terminal settings — reproduces the termios a real
    // `tmux -CC` leaves on the PTY (raw + ICRNL input, OPOST/ONLCR output).
    // This may fail if stdin is not a TTY (e.g., piped input in tests) —
    // in that case, just continue without the guard.
    let _term_guard = ControlModeGuard::enter(libc::STDIN_FILENO).ok();

    // Send IdentifyControl.
    let msg = proto::encode_client(&ClientMsg::IdentifyControl { rows, cols });
    proto::send(&mut stream, &msg)?;

    // Probe outer TTY defaults via /dev/tty (stdout is the control channel).
    {
        let fg = crate::client::query_outer_osc_color_for_control(10, true)
            .and_then(|d| crate::term::parse_osc_color_reply(&d))
            .map(|(_, rgb)| rgb);
        let bg = crate::client::query_outer_osc_color_for_control(11, true)
            .and_then(|d| crate::term::parse_osc_color_reply(&d))
            .map(|(_, rgb)| rgb);
        if fg.is_some() || bg.is_some() {
            let msg = proto::encode_client(&ClientMsg::TermPalette { fg, bg });
            proto::send(&mut stream, &msg)?;
        }
    }

    let stream_fd = stream.as_raw_fd();
    set_nonblocking(stream_fd);

    // If we're inside a real tmux pane (e.g. byobu inside iTerm2), raw
    // control-mode bytes would be rendered as pane text instead of reaching
    // iTerm2. Wrap all output in tmux's DCS passthrough so the outer tmux
    // forwards it to the terminal unmodified.
    let passthrough = std::env::var_os("TMUX").is_some();
    if passthrough {
        // Requires allow-passthrough in the outer tmux (3.3a+).
        let _ = std::process::Command::new("tmux")
            .args(["set", "-g", "allow-passthrough", "on"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    // Emit the DCS startup sequence.
    // ESC P 1 0 0 0 p  —  tells iTerm2 to enter tmux control mode.
    // No ST (String Terminator) is needed — tmux sends this without one,
    // and iTerm2 treats everything after as control mode notifications.
    let stdout = io::stdout();
    let stdout_fd = stdout.as_raw_fd();
    set_nonblocking(stdout_fd);
    let mut dbg = CcDebug::open();
    if passthrough {
        dbg.log("OUT", b"<tmux-passthrough enabled>");
    }
    let mut stdout_buf: VecDeque<u8> = VecDeque::new();
    queue_out(&mut stdout_buf, passthrough, b"\x1bP1000p");
    dbg.log("OUT", b"\x1bP1000p");
    flush_fd(stdout_fd, &mut stdout_buf);

    // Buffered data waiting to be written to the server socket.
    let mut server_outbuf: VecDeque<u8> = VecDeque::new();
    // Buffer for partial server frames.
    let mut server_buf: Vec<u8> = Vec::new();

    let stdin_fd = libc::STDIN_FILENO;
    let mut stdin_buf: Vec<u8> = Vec::new();
    let mut stdin_eof = false;

    loop {
        // Build poll fds dynamically: always watch the server socket for
        // input; watch stdin until EOF; watch POLLOUT on any fd that has
        // buffered output pending.
        let mut fds = vec![libc::pollfd {
            fd: stream_fd,
            events: libc::POLLIN
                | if server_outbuf.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
            revents: 0,
        }];
        let stdin_idx = if !stdin_eof {
            fds.push(libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            });
            Some(fds.len() - 1)
        } else {
            None
        };
        let stdout_idx = if !stdout_buf.is_empty() {
            fds.push(libc::pollfd {
                fd: stdout_fd,
                events: libc::POLLOUT,
                revents: 0,
            });
            Some(fds.len() - 1)
        } else {
            None
        };

        let ret = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }

        // stdout writable → drain buffered output
        if let Some(i) = stdout_idx
            && fds[i].revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0
        {
            flush_fd(stdout_fd, &mut stdout_buf);
        }

        // stdin → server: read ALL available bytes at once (never blocks
        // mid-line), accumulate, and dispatch complete lines.
        if let Some(i) = stdin_idx {
            if fds[i].revents & libc::POLLIN != 0 {
                let mut chunk = [0u8; 4096];
                let n = unsafe { libc::read(stdin_fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
                if n > 0 {
                    dbg.log("IN ", &chunk[..n as usize]);
                    stdin_buf.extend_from_slice(&chunk[..n as usize]);
                    // ^C cancels the pending partial line — iTerm2 sends it
                    // unconditionally before the first command to clear any
                    // garbage in case we're actually at a shell prompt.
                    while let Some(pos) = stdin_buf.iter().position(|&b| b == 0x03) {
                        stdin_buf.drain(..=pos);
                    }
                    // Extract complete lines (terminated by \n or \r).
                    while let Some(pos) = stdin_buf.iter().position(|&b| b == b'\n' || b == b'\r') {
                        let line: Vec<u8> = stdin_buf.drain(..pos).collect();
                        stdin_buf.drain(..1); // consume the terminator
                        let line = String::from_utf8_lossy(&line);
                        let line = line.trim();
                        if !line.is_empty() {
                            dbg.log("CMD", line.as_bytes());
                            let msg = proto::encode_client(&ClientMsg::ControlCommand {
                                line: line.to_string(),
                            });
                            server_outbuf.extend(&msg);
                        }
                    }
                    // Flush queued commands immediately; POLLOUT handles
                    // leftovers if the socket can't take everything.
                    flush_fd(stream_fd, &mut server_outbuf);
                } else if n == 0 {
                    dbg.log("IN ", b"<eof>");
                    stdin_eof = true;
                } else {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EAGAIN) {
                        stdin_eof = true;
                    }
                }
            }
            if fds[i].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                // stdin closed — don't exit, just stop reading from it.
                // iTerm2 may still be waiting for notifications.
                dbg.log("IN ", b"<hup>");
                stdin_eof = true;
            }
        }

        // server socket writable → drain pending commands
        if fds[0].revents & libc::POLLOUT != 0 {
            flush_fd(stream_fd, &mut server_outbuf);
        }

        // server → stdout (decode frames, queue ControlNotify lines)
        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 8192];
            let n = unsafe { libc::read(stream_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                server_buf.extend_from_slice(&buf[..n as usize]);
                // Try to parse complete frames.
                while let Some(msg) = try_parse_server_frame(&mut server_buf)? {
                    match msg {
                        ServerMsg::ControlNotify { line } => {
                            dbg.log("SRV", line.as_bytes());
                            let mut out = line.as_bytes().to_vec();
                            out.push(b'\n');
                            queue_out(&mut stdout_buf, passthrough, &out);
                            flush_fd(stdout_fd, &mut stdout_buf);
                            // Check for %exit.
                            if line.starts_with("%exit") {
                                // Emit the DCS terminator like real tmux
                                // (ESC \ on control-mode exit), then quit.
                                queue_out(&mut stdout_buf, passthrough, b"\x1b\\");
                                flush_fd(stdout_fd, &mut stdout_buf);
                                drain_stdin_grace(&mut dbg);
                                return Ok(());
                            }
                        }
                        ServerMsg::IdentifyAck { .. } => {
                            // Acknowledged — nothing to do for control mode.
                        }
                        ServerMsg::TermOscQuery {
                            pane_id,
                            code,
                            bell_terminated,
                        } => {
                            // iTerm2 -CC: stdout is the control channel, so
                            // query via /dev/tty (handled inside query helper).
                            if let Some(reply) = crate::client::query_outer_osc_color_for_control(
                                code,
                                bell_terminated,
                            ) {
                                let msg = proto::encode_client(&ClientMsg::TermOscReply {
                                    pane_id,
                                    data: reply,
                                });
                                server_outbuf.extend(&msg);
                                flush_fd(stream_fd, &mut server_outbuf);
                            }
                        }
                        ServerMsg::Error { msg } => {
                            // Don't print to stderr — iTerm2 would see it on
                            // the PTY. Just exit silently.
                            let _ = msg;
                            drain_stdin_grace(&mut dbg);
                            return Ok(());
                        }
                        _ => {
                            // Ignore other messages (grid updates, etc.) in
                            // control mode.
                        }
                    }
                }
            } else if n == 0 {
                // Server closed the connection — report it and exit.
                queue_out(&mut stdout_buf, passthrough, b"%exit\n");
                queue_out(&mut stdout_buf, passthrough, b"\x1b\\");
                flush_fd(stdout_fd, &mut stdout_buf);
                drain_stdin_grace(&mut dbg);
                return Ok(());
            }
        }

        // Check for server hangup
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            queue_out(&mut stdout_buf, passthrough, b"%exit\n");
            queue_out(&mut stdout_buf, passthrough, b"\x1b\\");
            flush_fd(stdout_fd, &mut stdout_buf);
            drain_stdin_grace(&mut dbg);
            return Ok(());
        }
    }
}

/// After %exit + DCS terminator, linger briefly draining stdin.
/// iTerm2 may still write queued tmux commands (e.g. `refresh-client -B`)
/// to the PTY for a short while after the control stream ends. If we exit
/// immediately those bytes land in the shell that spawned us and appear as
/// typed commands (`zsh: command not found: refresh-client`). While this
/// process stays alive reading stdin, those writes are eaten instead.
/// Exits once stdin has been quiet for 400ms, or after 1.5s at most.
fn drain_stdin_grace(dbg: &mut CcDebug) {
    let stdin_fd = libc::STDIN_FILENO;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    let mut last_data = std::time::Instant::now();
    loop {
        let quiet_for = last_data.elapsed();
        if quiet_for >= std::time::Duration::from_millis(400)
            || std::time::Instant::now() >= deadline
        {
            break;
        }
        let timeout_ms = (400 - quiet_for.as_millis()).max(1) as i32;
        let mut fds = [libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
        if ret <= 0 {
            continue;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            let mut chunk = [0u8; 4096];
            let n = unsafe { libc::read(stdin_fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
            if n > 0 {
                dbg.log("IN ", &chunk[..n as usize]);
                last_data = std::time::Instant::now();
            } else {
                break;
            }
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
    }
}

/// Queue bytes for stdout. When running inside a real tmux pane, wraps the
/// payload in tmux's DCS passthrough (`ESC Ptmux; <payload> ESC \`) so the
/// outer tmux forwards the raw bytes to iTerm2 instead of rendering them.
/// Inside the payload every ESC byte must be doubled.
fn queue_out(buf: &mut VecDeque<u8>, passthrough: bool, data: &[u8]) {
    if !passthrough {
        buf.extend(data);
        return;
    }
    buf.extend(b"\x1bPtmux;");
    for &b in data {
        if b == 0x1b {
            buf.extend(b"\x1b\x1b");
        } else {
            buf.push_back(b);
        }
    }
    buf.extend(b"\x1b\\");
}

/// Set a file descriptor to non-blocking mode.
fn set_nonblocking(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Write as much of the buffered output as possible to an fd.
/// Leftover bytes stay in the buffer and are retried on POLLOUT.
fn flush_fd(fd: libc::c_int, buf: &mut VecDeque<u8>) {
    while !buf.is_empty() {
        let (front, _) = buf.as_slices();
        let n = unsafe { libc::write(fd, front.as_ptr() as *const _, front.len()) };
        if n > 0 {
            buf.drain(..n as usize);
        } else {
            // EAGAIN or error — retry later.
            break;
        }
    }
}

/// Try to parse a complete server frame from the buffer.
/// Returns Ok(Some(msg)) if a complete frame was parsed, Ok(None) if incomplete.
fn try_parse_server_frame(buf: &mut Vec<u8>) -> io::Result<Option<ServerMsg>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return Ok(None);
    }
    // Extract the frame (including the 4-byte length prefix, since decode_server expects it).
    let frame: Vec<u8> = buf.drain(..4 + len).collect();
    let mut reader = &frame[..];
    proto::decode_server(&mut reader).map(Some)
}
