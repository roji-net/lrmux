// Pane: a rectangular region with a PTY running a child process, plus a grid.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::grid::{Cell, Grid};
use crate::pty::{Pty, PtySize, default_shell_argv};
use crate::vt;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use std::ffi::CString;
use std::path::PathBuf;

/// Global pane ID counter (tmux uses %N format).
static PANE_ID: AtomicU32 = AtomicU32::new(0);

/// Max time a query reply waits for the pane tty to leave cooked mode
/// before being dropped. Apps accept query replies only during a short
/// window after probing (devin ~150-200ms); a reply injected later lands in
/// the app's main input path and renders as typed garbage, so past the
/// deadline it's better to stay silent — a terminal that never answers is
/// something apps already handle via their own timeout.
const REPLY_DEFER_MAX: std::time::Duration = std::time::Duration::from_millis(100);

/// Minimum latency before a locally-generated reply is delivered. Real
/// terminals answer a query several ms after receiving it; a reply written
/// back sub-millisecond can land while the app is still reconfiguring
/// termios or before its reply reader is armed — devin then re-probes or
/// leaves the tail bytes to its main input path ("c11;rgb:…" in the
/// prompt). Holding replies for a few ms reproduces realistic timing.
const REPLY_MIN_DELAY: std::time::Duration = std::time::Duration::from_millis(8);

/// A reply slot in a pane's ordered deferred queue.
enum ReplySlot {
    /// Synthesized locally — bytes are final.
    Ready {
        earliest: std::time::Instant,
        deadline: std::time::Instant,
        bytes: Vec<u8>,
    },
    /// OSC color query proxied to a client; `fill_osc_reply` swaps in the
    /// real-TTY bytes when they arrive, keeping the slot's position.
    /// `code`/`bell` identify the query so an out-of-order client answer
    /// lands in the right slot and a late palette seed can synthesize the
    /// reply with the right terminator.
    Waiting {
        earliest: std::time::Instant,
        deadline: std::time::Instant,
        code: u8,
        bell: bool,
    },
}

impl ReplySlot {
    fn deadline(&self) -> std::time::Instant {
        match self {
            ReplySlot::Ready { deadline, .. } | ReplySlot::Waiting { deadline, .. } => *deadline,
        }
    }
}

/// A single pane: PTY + grid + VT parser.
pub struct Pane {
    pub id: u32,
    pub pty: Pty,
    pub grid: Grid,
    pub vt_parser: vte::Parser,
    pub rows: u16,
    pub cols: u16,
    /// True when the child process has exited and been reaped.
    pub exited: bool,
    /// Exit code of the child process (set when exited becomes true).
    /// Negative values indicate the child was killed by a signal (e.g. -9 for SIGKILL).
    pub exit_code: Option<i32>,
    /// Input bytes waiting to be written to the PTY master when it becomes writable.
    /// Prevents partial escape sequences when the child is slow to drain stdin.
    pub pending_input: Vec<u8>,
    /// Incomplete UTF-8 sequence at the end of the last PTY read, held so
    /// control-mode `%output` never splits a multi-byte character across
    /// notifications (which would turn `─` into `�`).
    pub cc_utf8_pending: Vec<u8>,
    /// Last known outer-terminal default foreground (OSC 10), if observed.
    pub default_fg: Option<(u8, u8, u8)>,
    /// Last known outer-terminal default background (OSC 11), if observed.
    pub default_bg: Option<(u8, u8, u8)>,
    /// Byte-level trace file for this pane (enabled by LRMUX_TRACE).
    trace: Option<std::fs::File>,
    /// Trace clock origin (pane spawn time).
    trace_start: std::time::Instant,
    /// Replies to child terminal queries (DA/CPR/OSC …) as an ordered
    /// queue of slots. `Ready` slots carry their bytes; `Waiting` slots
    /// are OSC queries proxied to a client — the reply is spliced in when
    /// it arrives so ordering vs. sibling queries is preserved (probe
    /// libraries like terminal-colorsaurus read responses positionally:
    /// a DA-first reply means "color query unsupported" and leaves the
    /// real color replies to render as typed text in the app).
    deferred_replies: Vec<ReplySlot>,
}

impl Pane {
    /// Spawn a new pane with the default shell.
    /// `session_id` is the owning session's id — exported to the child in
    /// the tmux-compat `TMUX` env var.
    pub fn new(rows: u16, cols: u16, session_id: u32) -> Self {
        let argv = default_shell_argv();
        Self::new_with_argv(rows, cols, &argv, None, session_id, &[])
    }

    /// Spawn a new pane with the default shell in a specific directory.
    pub fn new_in_cwd(rows: u16, cols: u16, cwd: &str, session_id: u32) -> Self {
        let argv = default_shell_argv();
        Self::new_with_argv(rows, cols, &argv, Some(cwd), session_id, &[])
    }

    /// Spawn a new pane with a custom command string.
    ///
    /// Runs `$SHELL -ci <command>` so interactive rc files (`.zshrc` /
    /// `.bashrc`) are sourced. Plain `-c` skips them and breaks vim
    /// truecolor / redraw (confirmed: `zsh -c vi` glitches, `zsh -ci vi`
    /// does not). After the command exits the shell exits, so the pane
    /// closes — no `exec` needed. Optional `cwd` sets the child's working
    /// directory (tmux `new-session -c`).
    pub fn new_with_command(
        rows: u16,
        cols: u16,
        command: &str,
        cwd: Option<&str>,
        session_id: u32,
        env: &[(String, String)],
    ) -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let cmd = command.trim();
        let argv = vec![
            CString::new(shell).unwrap(),
            // Combined `-ci` matches the working manual repro (`zsh -ci '…'`).
            // Separate `-i` `-c` should be equivalent; keep the proven form.
            CString::new("-ci").unwrap(),
            CString::new(cmd).unwrap(),
        ];
        Self::new_with_argv(rows, cols, &argv, cwd, session_id, env)
    }

    /// Spawn a new pane with the given argv and optional working directory.
    /// `env` carries extra variables for the child (tmux `new-window -e`).
    fn new_with_argv(
        rows: u16,
        cols: u16,
        argv: &[CString],
        cwd: Option<&str>,
        session_id: u32,
        env: &[(String, String)],
    ) -> Self {
        // Allocate the pane id before spawn so the tmux-compat env can carry it.
        let id = PANE_ID.fetch_add(1, Ordering::Relaxed);
        let mut extra_env = tmux_compat_env(session_id, id);
        // Own session in tmux $id target form: lets a nested `lrmux`
        // (NewWindowIn, etc.) address the session that spawned the pane
        // rather than defaulting to the server's first session. Ids are
        // stable across renames, unlike the session name.
        extra_env.push(("LRMUX_SESSION".into(), format!("${session_id}")));
        extra_env.extend(env.iter().cloned());
        let pty = Pty::spawn(argv, PtySize { rows, cols }, cwd, &extra_env);

        let mut trace = open_pane_trace(id);
        if let Some(ref mut f) = trace {
            use std::io::Write;
            let _ = writeln!(
                f,
                "# pane %{id} session ${session_id} argv={:?} size={}x{}",
                argv, cols, rows
            );
        }
        let grid = Grid::new(rows as usize, cols as usize, 10_000);
        let vt_parser = vte::Parser::new();
        Self {
            id,
            pty,
            grid,
            vt_parser,
            rows,
            cols,
            exited: false,
            exit_code: None,
            pending_input: Vec::new(),
            cc_utf8_pending: Vec::new(),
            default_fg: None,
            default_bg: None,
            trace,
            trace_start: std::time::Instant::now(),
            deferred_replies: Vec::new(),
        }
    }

    /// Palette learned from proxied OSC 10/11 replies (for HTML capture).
    pub fn terminal_palette(&self) -> super::capture::TerminalPalette {
        super::capture::TerminalPalette {
            fg: self.default_fg,
            bg: self.default_bg,
        }
    }

    /// Record an OSC 10/11 color reply from the outer TTY.
    pub fn note_osc_color_reply(&mut self, data: &[u8]) {
        if let Some((code, rgb)) = crate::term::parse_osc_color_reply(data) {
            match code {
                10 => self.default_fg = Some(rgb),
                11 => self.default_bg = Some(rgb),
                _ => {}
            }
        }
    }

    /// Coalesce `raw` with any incomplete UTF-8 left from the previous read.
    /// Returns bytes safe to forward as `%output` (no trailing incomplete
    /// sequence). Leftover incomplete bytes stay in `cc_utf8_pending`.
    pub fn take_cc_forward_bytes(&mut self, raw: &[u8]) -> Vec<u8> {
        if self.cc_utf8_pending.is_empty() && raw.is_empty() {
            return Vec::new();
        }
        self.cc_utf8_pending.extend_from_slice(raw);
        let incomplete = incomplete_utf8_tail_len(&self.cc_utf8_pending);
        let cut = self.cc_utf8_pending.len() - incomplete;
        let complete = self.cc_utf8_pending[..cut].to_vec();
        self.cc_utf8_pending.drain(..cut);
        complete
    }

    /// Flush any buffered incomplete UTF-8 (pane exit / final drain).
    pub fn flush_cc_forward_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.cc_utf8_pending)
    }

    /// Format the pane ID as tmux-style: %N
    pub fn id_str(&self) -> String {
        format!("%{}", self.id)
    }

    /// Trace a non-byte event (resize, exit, …) as an EVT line.
    pub fn trace_event(&mut self, msg: &str) {
        self.trace("EVT", msg.as_bytes());
    }

    /// Append one timestamped line to this pane's trace file, when enabled
    /// (`LRMUX_TRACE`). Tags: IN = client keystrokes→PTY, OUT = PTY→grid,
    /// RPL = our replies to the child's terminal queries.
    fn trace(&mut self, tag: &str, data: &[u8]) {
        if let Some(ref mut f) = self.trace {
            use std::io::Write;
            let ms = self.trace_start.elapsed().as_secs_f64() * 1000.0;
            let _ = writeln!(
                f,
                "[+{ms:>11.3}ms] {tag} len={:5} | {}",
                data.len(),
                crate::log::vis_bytes(data)
            );
        }
    }

    /// Get the PTY master fd (for poll). Returns -1 if the child has exited.
    pub fn pty_fd(&self) -> i32 {
        if self.exited {
            -1
        } else {
            self.pty.master_fd()
        }
    }

    /// Read PTY output, parse into grid. Returns (still_alive, raw_bytes,
    /// unanswered_osc_queries). raw_bytes is the unprocessed output from the
    /// PTY (for control mode forwarding). OSC 10/11 queries we can't answer
    /// from the pane's or `palette_fallback`'s cached colors come back so the
    /// caller can proxy them to an attached client TTY.
    ///
    /// All replies generated in this pass (DA/CPR + OSC answers) go out in a
    /// single write at the end: apps typically accept replies only during a
    /// short raw-mode slice right after querying (devin ~15-25ms), so a flush
    /// split by a termios flip mid-batch turns the second half into typed
    /// garbage.
    pub fn process_pty_output(
        &mut self,
        palette_fallback: super::capture::TerminalPalette,
    ) -> io::Result<(bool, Vec<u8>, Vec<vt::OscColorQuery>)> {
        if self.exited {
            return Ok((false, Vec::new(), Vec::new()));
        }
        // Drain the PTY until EAGAIN so a burst becomes a single grid
        // update — one 8KB read per poll event would generate a frame per
        // chunk and flood slow clients past their socket buffer cap.
        // Cap at 4MB per event so an endless producer can't starve the
        // event loop (poll refires while data remains).
        const MAX_DRAIN: usize = 4 * 1024 * 1024;
        let mut raw = Vec::new();
        let mut osc_queries = Vec::new();
        let mut buf = [0u8; 65536];
        let fd = self.pty_fd();
        let mut alive = true;
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let n = n as usize;
                raw.extend_from_slice(&buf[..n]);
                self.trace("OUT", &buf[..n]);
                if let Ok(path) = std::env::var("LRMUX_VT_DUMP") {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        let _ = f.write_all(&buf[..n]);
                    }
                }
                let parsed = vt::parse_bytes(&mut self.vt_parser, &mut self.grid, &buf[..n]);
                // Replies must leave in the order the child asked: probe
                // libraries (terminal-colorsaurus, used by devin) read
                // responses sequentially and treat a DA-first reply as
                // "color queries unsupported", leaking the real color
                // replies into the app's input. OSC queries with no cached
                // answer get a `Waiting` slot so the proxied reply is
                // spliced into position rather than appended at the end.
                for pending in parsed.replies {
                    match pending {
                        vt::PendingReply::Bytes(b) => self.queue_ready_reply(b),
                        vt::PendingReply::Osc(q) => {
                            match self.osc_color_reply(&q, palette_fallback) {
                                Some(r) => self.queue_ready_reply(r),
                                None => {
                                    self.queue_waiting_reply(&q);
                                    osc_queries.push(q);
                                }
                            }
                        }
                    }
                }
                if raw.len() >= MAX_DRAIN {
                    break;
                }
                continue;
            }
            if n == 0 {
                alive = false;
                break;
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                break;
            } else if err.raw_os_error() == Some(libc::EIO) {
                alive = false;
                break;
            } else {
                return Err(err);
            }
        }
        let _ = self.flush_deferred_replies();
        Ok((alive, raw, osc_queries))
    }

    /// Synthesize an OSC 10/11 reply from the pane's cached palette or the
    /// fallback (an attached client's probed colors). None = unknown, the
    /// caller should proxy the query to a real TTY.
    fn osc_color_reply(
        &self,
        q: &vt::OscColorQuery,
        fallback: super::capture::TerminalPalette,
    ) -> Option<Vec<u8>> {
        let rgb = match q.code {
            10 => self.default_fg.or(fallback.fg),
            11 => self.default_bg.or(fallback.bg),
            _ => None,
        }?;
        Some(crate::term::format_osc_color_reply(q.code, rgb, q.bell_terminated).into_bytes())
    }

    /// Reap the child process and store the exit code.
    /// Should be called after `process_pty_output` returns `Ok(false)`.
    /// Returns the exit code if the child was reaped, or None if still alive.
    ///
    /// On macOS, the PTY master fd may return EOF/EIO before the child is
    /// fully reaped by the OS. We retry waitpid a few times with small delays
    /// to handle this race condition.
    pub fn reap_child(&mut self) -> Option<i32> {
        if self.exited {
            return self.exit_code;
        }
        for _ in 0..20 {
            match waitpid(self.pty.child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => {
                    self.exited = true;
                    self.exit_code = Some(code);
                    return Some(code);
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    let code = -(sig as i32);
                    self.exited = true;
                    self.exit_code = Some(code);
                    return Some(code);
                }
                Ok(WaitStatus::StillAlive) => {
                    // Child hasn't been reaped yet — wait briefly and retry.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                _ => return None,
            }
        }
        // Timed out waiting for reap — treat as exit code 0.
        self.exited = true;
        self.exit_code = Some(0);
        Some(0)
    }

    /// Write a "[process exited, code N]" message to the grid.
    /// Uses red text (SGR 31) so it stands out from normal output.
    pub fn write_exit_message(&mut self, code: i32) {
        let msg = if code < 0 {
            format!("\r\n\x1b[31m[process exited, signal {}]\x1b[0m\r\n", -code)
        } else {
            format!("\r\n\x1b[31m[process exited, code {}]\x1b[0m\r\n", code)
        };
        let _ = vt::parse_bytes(&mut self.vt_parser, &mut self.grid, msg.as_bytes());
    }

    /// Check if the pane's child has exited.
    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// Write input bytes to the PTY (keystrokes from client).
    /// The master fd is nonblocking. If the child is slow to drain stdin,
    /// the remaining bytes are queued in `pending_input` and flushed by the
    /// event loop when the PTY becomes writable. This keeps escape sequences
    /// intact (e.g., arrow keys) instead of splitting them across writes.
    pub fn write_input(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_input_tagged("IN", data)
    }

    /// `write_input` with a trace tag distinguishing the byte source
    /// ("IN" = client input, "RPL" = our terminal-query replies).
    pub(crate) fn write_input_tagged(&mut self, tag: &str, data: &[u8]) -> io::Result<()> {
        self.trace(tag, data);
        self.pending_input.extend_from_slice(data);
        self.flush_pending_input()?;
        // `flush_pending_input` holds an incomplete ESC sequence until more
        // bytes arrive (so CSI can be written atomically). A write_input of
        // a lone ESC (Esc key) would otherwise sit forever — flush it now.
        // Control-mode send-keys coalescing writes full sequences in one
        // call, so this path does not re-split arrow keys.
        if self.pending_input.len() == 1 && self.pending_input[0] == 0x1b {
            let fd = self.pty_fd();
            if fd >= 0 {
                let w = unsafe { libc::write(fd, self.pending_input.as_ptr() as *const _, 1) };
                if w > 0 {
                    self.pending_input.clear();
                } else if w < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::WouldBlock {
                        self.pending_input.clear();
                        return Err(err);
                    }
                }
            }
        }
        Ok(())
    }

    /// Try to drain `pending_input` to the PTY master.
    /// Returns Ok when the buffer is empty or the fd is not yet writable.
    ///
    /// Never ends a write in the middle of an ANSI/CSI escape sequence: if
    /// the buffer starts with an incomplete ESC sequence, wait for more bytes
    /// or POLLOUT (EAGAIN with 0 bytes written is fine). ASCII runs still go
    /// out in bulk.
    pub fn flush_pending_input(&mut self) -> io::Result<()> {
        let fd = self.pty_fd();
        if fd < 0 || self.pending_input.is_empty() {
            return Ok(());
        }
        while !self.pending_input.is_empty() {
            let want = input_write_len(&self.pending_input);
            if want == 0 {
                // Incomplete ESC/CSI at the head — wait for more input.
                return Ok(());
            }
            let w = unsafe { libc::write(fd, self.pending_input.as_ptr() as *const _, want as _) };
            if w < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                self.pending_input.clear();
                return Err(err);
            }
            if w == 0 {
                return Ok(());
            }
            let n = w as usize;
            // If the kernel accepted a mid-sequence prefix, still drain what
            // was written (can't un-write); prefer small complete units so
            // this is rare.
            if n >= self.pending_input.len() {
                self.pending_input.clear();
            } else {
                self.pending_input.drain(..n);
            }
        }
        Ok(())
    }

    /// Queue a synthesized reply in FIFO position (same slot queue used by
    /// `process_pty_output`, so it stays ordered relative to any pending
    /// OSC answers from the same probe round).
    fn queue_ready_reply(&mut self, bytes: Vec<u8>) {
        self.trace("DFR", &bytes);
        let now = std::time::Instant::now();
        self.deferred_replies.push(ReplySlot::Ready {
            earliest: now + REPLY_MIN_DELAY,
            deadline: now + REPLY_DEFER_MAX,
            bytes,
        });
    }

    /// Queue a placeholder for an OSC query that had no cached palette
    /// answer — the event loop proxies the query to a client and
    /// `fill_osc_reply` splices the real bytes into this slot.
    fn queue_waiting_reply(&mut self, q: &vt::OscColorQuery) {
        self.trace("DFR", b"waiting for proxied OSC reply");
        let now = std::time::Instant::now();
        self.deferred_replies.push(ReplySlot::Waiting {
            earliest: now + REPLY_MIN_DELAY,
            deadline: now + REPLY_DEFER_MAX,
            code: q.code,
            bell: q.bell_terminated,
        });
    }

    /// A palette seed landed while queries were in flight (client's
    /// TermPalette raced the app's query): synthesize answers for any
    /// still-waiting slots instead of waiting out the proxied roundtrip.
    pub fn resolve_waiting_from_palette(
        &mut self,
        fallback: super::capture::TerminalPalette,
    ) -> io::Result<()> {
        let fg = self.default_fg.or(fallback.fg);
        let bg = self.default_bg.or(fallback.bg);
        let mut filled = false;
        for s in &mut self.deferred_replies {
            let ReplySlot::Waiting {
                earliest,
                deadline,
                code,
                bell,
            } = *s
            else {
                continue;
            };
            let rgb = match code {
                10 => fg,
                11 => bg,
                _ => None,
            };
            if let Some(rgb) = rgb {
                *s = ReplySlot::Ready {
                    earliest,
                    deadline,
                    bytes: crate::term::format_osc_color_reply(code, rgb, bell).into_bytes(),
                };
                filled = true;
            }
        }
        if filled {
            self.trace("DFR", b"filled waiting reply from seeded palette");
            self.flush_deferred_replies()?;
        }
        Ok(())
    }

    /// Write a reply to a child terminal query (DA/CPR/OSC …).
    ///
    /// The reply is queued and flushed by `flush_deferred_replies` once it
    /// has aged past REPLY_MIN_DELAY and the pane tty is no longer cooked.
    /// A cooked tty (ICANON or ECHO) echoes the reply as "^[…" garbage and
    /// canonical-buffers it past the app's read window, so it resurfaces as
    /// literal input once the app goes raw (devin's prompt showing
    /// "10;rgb:…"). Replies that outlive REPLY_DEFER_MAX are dropped — a
    /// terminal that never answers is safer than one that answers late.
    pub fn write_reply(&mut self, data: &[u8]) -> io::Result<()> {
        self.queue_ready_reply(data.to_vec());
        self.flush_deferred_replies()
    }

    /// A client sent the real-TTY answer to a proxied OSC color query:
    /// fill the waiting slot for that query code so the batch flushes in
    /// query order. A reply with no waiting slot is too old to matter —
    /// drop it rather than inject an out-of-order/late reply.
    pub fn fill_osc_reply(&mut self, data: &[u8]) -> io::Result<()> {
        let now = std::time::Instant::now();
        // The reply carries its code ("\x1b]10;rgb:…"); match by code so a
        // client answering out of order still lands in the right slot.
        let code = data
            .get(2..4)
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.parse::<u8>().ok());
        let Some(i) = self
            .deferred_replies
            .iter()
            .position(|s| matches!(s, ReplySlot::Waiting { code: c, .. } if Some(*c) == code))
        else {
            self.trace("DFR", b"dropping stray proxied reply");
            return self.flush_deferred_replies();
        };
        let ReplySlot::Waiting {
            earliest, deadline, ..
        } = self.deferred_replies[i]
        else {
            unreachable!()
        };
        if now < deadline {
            self.trace("DFR", b"proxied reply arrived");
            self.deferred_replies[i] = ReplySlot::Ready {
                earliest,
                deadline,
                bytes: data.to_vec(),
            };
        } else {
            self.trace("DFR", b"dropping late proxied reply");
            // leave the slot; the flush pass below expires it
        }
        self.flush_deferred_replies()
    }

    /// Inject deferred replies once they are old enough (REPLY_MIN_DELAY),
    /// ordered, and the tty has left cooked mode. Slots that outlived their
    /// deadline are dropped. Called every event-loop pass — the child's
    /// tcsetattr produces no poll event, so the loop clamps its timeout
    /// while `has_deferred_replies()`.
    pub fn flush_deferred_replies(&mut self) -> io::Result<()> {
        let now = std::time::Instant::now();
        // Expired slots are never written — cooked or not. A reply that
        // couldn't be delivered within REPLY_DEFER_MAX would land after the
        // child's read window and echo back as typed text.
        while self
            .deferred_replies
            .first()
            .is_some_and(|s| now >= s.deadline())
        {
            self.trace("DFR", b"dropping reply: deadline expired");
            self.deferred_replies.remove(0);
        }
        if self.deferred_replies.is_empty() {
            return Ok(());
        }
        if self.tty_cooked() {
            self.trace(
                "DFR",
                format!(
                    "holding {} repl(ies), tty cooked lflag={:#x}",
                    self.deferred_replies.len(),
                    self.tty_lflag()
                )
                .as_bytes(),
            );
            return Ok(());
        }
        // Deliver the contiguous run of aged, ready slots as ONE write —
        // a Waiting slot (proxied OSC) is an order barrier: replies after
        // it must not overtake it.
        let mut batch = Vec::new();
        let mut n = 0;
        for slot in &self.deferred_replies {
            match slot {
                ReplySlot::Ready {
                    earliest, bytes, ..
                } if now >= *earliest => {
                    batch.extend_from_slice(bytes);
                    n += 1;
                }
                _ => break,
            }
        }
        if n == 0 {
            self.trace(
                "DFR",
                format!(
                    "holding {} repl(ies), waiting on proxy/min-delay",
                    self.deferred_replies.len()
                )
                .as_bytes(),
            );
            return Ok(());
        }
        self.trace("DFR", format!("flushing {n} repl(ies)").as_bytes());
        self.deferred_replies.drain(..n);
        self.write_input_tagged("RPL", &batch)?;
        Ok(())
    }

    /// The event loop had no client to proxy a queued OSC query to —
    /// drop the oldest waiting slot so trailing replies aren't held
    /// hostage for the full deadline.
    pub fn drop_waiting_reply(&mut self) {
        if let Some(i) = self
            .deferred_replies
            .iter()
            .position(|s| matches!(s, ReplySlot::Waiting { .. }))
        {
            self.trace("DFR", b"dropping waiting slot: no proxy target");
            self.deferred_replies.remove(i);
        }
        let _ = self.flush_deferred_replies();
    }

    /// Replies still waiting for the tty to leave cooked mode.
    pub fn has_deferred_replies(&self) -> bool {
        !self.deferred_replies.is_empty()
    }

    /// True while the pane tty is in cooked mode: canonical input and/or
    /// echo. tcgetattr on the master fd reflects the slave's termios.
    fn tty_cooked(&self) -> bool {
        self.tty_lflag() & (libc::ICANON | libc::ECHO) != 0
    }

    /// Raw c_lflag of the pane tty (for trace diagnostics), 0 on error.
    fn tty_lflag(&self) -> u64 {
        let fd = self.pty_fd();
        if fd < 0 {
            return 0;
        }
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        match nix::sys::termios::tcgetattr(borrowed) {
            Ok(t) => t.local_flags.bits(),
            Err(_) => 0,
        }
    }

    /// Resize the pane (PTY + grid).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.pty.resize(PtySize { rows, cols });
        self.grid.resize(rows as usize, cols as usize);
        self.rows = rows;
        self.cols = cols;
    }

    /// Collect dirty rows as (row_index, cells) pairs for protocol transmission.
    pub fn take_dirty_rows(&mut self) -> Vec<(u16, Vec<Cell>)> {
        let dirty = self.grid.take_dirty();
        let cols = self.cols as usize;
        dirty
            .into_iter()
            .filter_map(|row| {
                if row >= self.rows as usize {
                    return None;
                }
                let cells: Vec<Cell> = self
                    .grid
                    .row(row)
                    .map(|r| {
                        r.iter()
                            .take(cols)
                            .map(|c| {
                                if c.ch == '\0' {
                                    Cell {
                                        ch: ' ',
                                        fg: c.fg,
                                        bg: c.bg,
                                        attrs: c.attrs,
                                    }
                                } else {
                                    c.clone()
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some((row as u16, cells))
            })
            .collect()
    }

    /// Take pending scrollback rows for client synchronization.
    pub fn take_pending_scrollback(&mut self) -> Vec<Vec<Cell>> {
        let cols = self.cols as usize;
        self.grid
            .take_pending_scrollback()
            .into_iter()
            .map(|row| {
                row.iter()
                    .take(cols)
                    .map(|c| {
                        if c.ch == '\0' {
                            Cell {
                                ch: ' ',
                                fg: c.fg,
                                bg: c.bg,
                                attrs: c.attrs,
                            }
                        } else {
                            c.clone()
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// Get a full grid snapshot as a flat cell vector (row-major).
    pub fn snapshot(&self) -> Vec<Cell> {
        let mut cells = Vec::with_capacity((self.rows as usize) * (self.cols as usize));
        for row in 0..self.rows as usize {
            if let Some(r) = self.grid.row(row) {
                for c in r.iter().take(self.cols as usize) {
                    if c.ch == '\0' {
                        cells.push(Cell {
                            ch: ' ',
                            fg: c.fg,
                            bg: c.bg,
                            attrs: c.attrs,
                        });
                    } else {
                        cells.push(c.clone());
                    }
                }
            } else {
                for _ in 0..self.cols {
                    cells.push(Cell::blank());
                }
            }
        }
        cells
    }

    /// Get cursor position and visibility.
    pub fn cursor(&self) -> (u16, u16, bool) {
        (
            self.grid.cursor_row as u16,
            self.grid.cursor_col as u16,
            self.grid.cursor_visible,
        )
    }

    /// Get the full scrollback as a vector of rows (oldest first).
    /// Used when sending a snapshot to a client (window switch, initial connect).
    pub fn scrollback_rows(&self) -> Vec<Vec<Cell>> {
        let cols = self.cols as usize;
        self.grid
            .scrollback
            .iter()
            .map(|row| {
                row.iter()
                    .take(cols)
                    .map(|c| {
                        if c.ch == '\0' {
                            Cell {
                                ch: ' ',
                                fg: c.fg,
                                bg: c.bg,
                                attrs: c.attrs,
                            }
                        } else {
                            c.clone()
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

/// How many leading bytes of `buf` are safe to write without splitting an
/// ANSI escape sequence. Returns 0 if the buffer starts with an incomplete
/// ESC sequence (caller should wait for more data / POLLOUT).
fn input_write_len(buf: &[u8]) -> usize {
    let mut pos = 0;
    while pos < buf.len() {
        if buf[pos] != 0x1b {
            // Bulk ASCII until the next ESC (or end of buffer).
            let rest = &buf[pos..];
            pos += rest.iter().position(|&b| b == 0x1b).unwrap_or(rest.len());
            continue;
        }
        match ansi_input_seq_len(&buf[pos..]) {
            Some(n) => pos += n,
            None => break,
        }
    }
    pos
}

/// Length of a complete ANSI input escape sequence at the start of `buf`,
/// or None if more bytes are needed.
///
/// Recognizes CSI (`ESC [ ... final 0x40-0x7E`), SS3 (`ESC O X`), and
/// other two-byte ESC sequences. A lone ESC waits for at least one more
/// byte so split send-keys (`ESC` then `[` then `D`) can coalesce.
fn ansi_input_seq_len(buf: &[u8]) -> Option<usize> {
    if buf.is_empty() || buf[0] != 0x1b {
        return None;
    }
    if buf.len() < 2 {
        return None;
    }
    match buf[1] {
        b'[' => {
            // CSI: parameters/intermediates until a final byte 0x40-0x7E.
            for (i, &b) in buf.iter().enumerate().skip(2) {
                if (0x40..=0x7e).contains(&b) {
                    return Some(i + 1);
                }
            }
            None
        }
        b']' => {
            // OSC: ESC ] ... BEL or ST (ESC \). Used for term color replies.
            let mut i = 2;
            while i < buf.len() {
                if buf[i] == 0x07 {
                    return Some(i + 1);
                }
                if buf[i] == 0x1b {
                    if i + 1 < buf.len() && buf[i + 1] == b'\\' {
                        return Some(i + 2);
                    }
                    return None;
                }
                i += 1;
            }
            None
        }
        b'O' => {
            // SS3: ESC O X
            if buf.len() >= 3 { Some(3) } else { None }
        }
        _ => Some(2), // ESC + one more
    }
}

/// Number of trailing bytes that form an incomplete UTF-8 sequence.
/// Returns 0 if the buffer ends on a complete character boundary (or with
/// orphaned continuation bytes that should be escaped, not buffered).
pub(crate) fn incomplete_utf8_tail_len(data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    // Prefer std's verdict when the only problem is an incomplete suffix.
    match std::str::from_utf8(data) {
        Ok(_) => 0,
        Err(e) if e.error_len().is_none() => data.len() - e.valid_up_to(),
        Err(_) => {
            // Invalid mid-stream (or orphaned continuations). Only buffer a
            // trailing incomplete *starter* sequence; never hold garbage.
            let len = data.len();
            let lookback = len.min(3);
            for i in 1..=lookback {
                let idx = len - i;
                let needed = match data[idx] {
                    0x00..=0x7F => 1,
                    0xC2..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    0xF0..=0xF4 => 4,
                    _ => 0, // continuation / invalid
                };
                if needed == 0 {
                    continue;
                }
                if needed > i {
                    return i;
                }
                return 0;
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_tail_empty_and_ascii() {
        assert_eq!(incomplete_utf8_tail_len(b""), 0);
        assert_eq!(incomplete_utf8_tail_len(b"hello"), 0);
    }

    #[test]
    fn incomplete_tail_box_drawing() {
        // ─ is E2 94 80
        assert_eq!(incomplete_utf8_tail_len(&[0xE2, 0x94, 0x80]), 0);
        assert_eq!(incomplete_utf8_tail_len(&[0xE2]), 1);
        assert_eq!(incomplete_utf8_tail_len(&[0xE2, 0x94]), 2);
        assert_eq!(incomplete_utf8_tail_len(&[b'-', 0xE2, 0x94]), 2);
    }

    #[test]
    fn take_cc_forward_coalesces_across_reads() {
        // ─ split as [E2] + [94 80]: first read buffers, second emits the char.
        let mut pending = vec![0xE2];
        let incomplete = incomplete_utf8_tail_len(&pending);
        assert_eq!(incomplete, 1);
        pending.extend_from_slice(&[0x94, 0x80, b'x']);
        assert_eq!(incomplete_utf8_tail_len(&pending), 0);
        assert_eq!(&pending[..], &[0xE2, 0x94, 0x80, b'x']);
    }

    #[test]
    fn ansi_input_seq_len_recognizes_osc() {
        let osc = b"\x1b]11;rgb:0000/0000/0000\x07";
        assert_eq!(ansi_input_seq_len(osc), Some(osc.len()));
        // Incomplete OSC waits.
        assert_eq!(ansi_input_seq_len(b"\x1b]11;rgb:0000"), None);
        let st = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
        assert_eq!(ansi_input_seq_len(st), Some(st.len()));
    }

    #[test]
    fn vis_bytes_escapes_controls_keeps_utf8() {
        use crate::log::vis_bytes;
        assert_eq!(vis_bytes(b"hi\r\n"), "hi\\r\\n");
        assert_eq!(vis_bytes(b"\x1b[6n"), "\\e[6n");
        assert_eq!(vis_bytes(b"\x07bel\x01"), "\\abel\\x01");
        // Box-drawing char ─ stays a char, not three \xNN escapes.
        assert_eq!(vis_bytes("─x".as_bytes()), "─x");
        // Invalid UTF-8 byte -> hex escape; C1 control -> \u{..}
        assert_eq!(vis_bytes(&[0xff]), "\\xff");
        assert_eq!(vis_bytes(&[0xc2, 0x9b]), "\\u{9b}");
    }

    fn osc_q(code: u8) -> vt::OscColorQuery {
        vt::OscColorQuery {
            code,
            bell_terminated: true,
        }
    }

    /// /bin/cat echoes replies back, so the test can observe the exact
    /// byte order written to the pty. The slave is forced raw via the
    /// master fd so the cooked check doesn't hold replies.
    fn cat_pane() -> Pane {
        let argv = [CString::new("/bin/cat").unwrap()];
        let pane = Pane::new_with_argv(24, 80, &argv, None, 0, &[]);
        let fd = pane.pty.master_fd();
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd, &mut t), 0);
            t.c_lflag &= !(libc::ICANON | libc::ECHO);
            assert_eq!(libc::tcsetattr(fd, libc::TCSANOW, &t), 0);
        }
        pane
    }

    /// Bytes echoed back by cat after replies were written to the master.
    fn read_master(pane: &Pane) -> Vec<u8> {
        let fd = pane.pty.master_fd();
        let mut out = Vec::new();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 1000) } <= 0 {
            return out;
        }
        loop {
            let mut buf = [0u8; 4096];
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            pfd.revents = 0;
            if unsafe { libc::poll(&mut pfd, 1, 100) } <= 0 {
                break;
            }
        }
        out
    }

    /// Backdate `earliest` so slots are flushable without sleeping.
    fn age_all(pane: &mut Pane) {
        for s in &mut pane.deferred_replies {
            match s {
                ReplySlot::Ready { earliest, .. } | ReplySlot::Waiting { earliest, .. } => {
                    *earliest = std::time::Instant::now() - std::time::Duration::from_secs(1);
                }
            }
        }
    }

    #[test]
    fn replies_flush_in_query_order() {
        // The app asked OSC 10, OSC 11, DA — answers must go out in that
        // order even though the proxied colors resolve asynchronously and
        // may arrive out of order (11 before 10).
        let mut pane = cat_pane();
        pane.queue_waiting_reply(&osc_q(10));
        pane.queue_waiting_reply(&osc_q(11));
        pane.queue_ready_reply(b"\x1b[?62;1;2c".to_vec());
        pane.fill_osc_reply(b"\x1b]11;rgb:1111/2222/3333\x07")
            .unwrap();
        pane.fill_osc_reply(b"\x1b]10;rgb:aaaa/bbbb/cccc\x07")
            .unwrap();
        age_all(&mut pane);
        pane.flush_deferred_replies().unwrap();
        assert!(pane.deferred_replies.is_empty());
        assert_eq!(
            read_master(&pane),
            b"\x1b]10;rgb:aaaa/bbbb/cccc\x07\x1b]11;rgb:1111/2222/3333\x07\x1b[?62;1;2c"
        );
    }

    #[test]
    fn waiting_slot_blocks_trailing_replies() {
        // While a proxied OSC is unanswered, later ready replies must not
        // overtake it — the whole contiguous prefix flushes together.
        let mut pane = cat_pane();
        pane.queue_waiting_reply(&osc_q(10));
        pane.queue_ready_reply(b"\x1b[?62c".to_vec());
        age_all(&mut pane);
        pane.flush_deferred_replies().unwrap();
        assert_eq!(read_master(&pane), b"", "DA overtook the waiting OSC");
        assert_eq!(pane.deferred_replies.len(), 2);
    }

    #[test]
    fn stray_proxied_reply_dropped() {
        // An OSC reply with no waiting slot is stale — never injected.
        let mut pane = cat_pane();
        pane.fill_osc_reply(b"\x1b]10;rgb:aaaa/bbbb/cccc\x07")
            .unwrap();
        assert!(pane.deferred_replies.is_empty());
        assert_eq!(read_master(&pane), b"");
    }

    #[test]
    fn drop_waiting_reply_unblocks_queue() {
        // No client to proxy to: the waiting slot is removed so the
        // trailing DA flushes instead of waiting out the deadline.
        let mut pane = cat_pane();
        pane.queue_waiting_reply(&osc_q(10));
        pane.queue_ready_reply(b"\x1b[?62c".to_vec());
        age_all(&mut pane);
        pane.drop_waiting_reply();
        assert!(pane.deferred_replies.is_empty());
        assert_eq!(read_master(&pane), b"\x1b[?62c");
    }

    #[test]
    fn palette_seed_resolves_waiting_replies() {
        // Cold-start: the app's queries landed before the client's
        // TermPalette. Seeding the palette resolves the waiting slots in
        // place — no client roundtrip needed.
        let mut pane = cat_pane();
        pane.queue_waiting_reply(&osc_q(10));
        pane.queue_waiting_reply(&osc_q(11));
        pane.queue_ready_reply(b"\x1b[?62;1;2c".to_vec());
        pane.default_fg = Some((0xaa, 0xbb, 0xcc));
        pane.default_bg = Some((0x11, 0x22, 0x33));
        age_all(&mut pane);
        pane.resolve_waiting_from_palette(crate::server::capture::TerminalPalette::default())
            .unwrap();
        assert!(pane.deferred_replies.is_empty());
        assert_eq!(
            read_master(&pane),
            b"\x1b]10;rgb:aaaa/bbbb/cccc\x07\x1b]11;rgb:1111/2222/3333\x07\x1b[?62;1;2c"
        );
    }

    #[test]
    fn expired_reply_dropped_not_written() {
        // A reply that outlived its deadline is discarded — injecting it
        // late would echo back as typed text in the app.
        let mut pane = cat_pane();
        pane.queue_ready_reply(b"\x1b]11;rgb:0000/0000/0000\x07".to_vec());
        match &mut pane.deferred_replies[0] {
            ReplySlot::Ready { deadline, .. } => {
                *deadline = std::time::Instant::now() - std::time::Duration::from_secs(1);
            }
            _ => unreachable!(),
        }
        pane.flush_deferred_replies().unwrap();
        assert!(pane.deferred_replies.is_empty());
        assert_eq!(read_master(&pane), b"");
    }

    #[test]
    fn tmux_env_pairs_match_tmux_shape() {
        let env = tmux_env_pairs("/tmp/lrmux-501/srv", 12345, 7, 3);
        assert_eq!(env.len(), 4);
        assert_eq!(env[0].0, "TMUX");
        assert_eq!(env[0].1, "/tmp/lrmux-501/srv,12345,7");
        // tmux shape: <socket path>,<server pid>,<session id>
        let fields: Vec<&str> = env[0].1.split(',').collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(env[1], ("TMUX_PANE".into(), "%3".into()));
        // tmux also advertises itself via TERM_PROGRAM
        assert_eq!(env[2], ("TERM_PROGRAM".into(), "tmux".into()));
        assert_eq!(env[3].0, "TERM_PROGRAM_VERSION");
    }
}

/// Extra env exported to pane children in tmux-compat mode
/// (`[behavior] tmux_compat` / `--tmux-compat`): `TMUX` carries
/// `<socket>,<server pid>,<session id>` (same shape as tmux),
/// `TMUX_PANE` is the `%N` pane id, and `TERM_PROGRAM`/`TERM_PROGRAM_VERSION`
/// identify the mux as tmux — as real tmux does — so apps that detect a
/// multiplexer env see a consistent picture.
fn tmux_compat_env(session_id: u32, pane_id: u32) -> Vec<(String, String)> {
    if !crate::config::tmux_compat() {
        return Vec::new();
    }
    let sock = crate::ipc::socket_path(crate::server::server_name());
    let mut env = tmux_env_pairs(
        &sock.display().to_string(),
        std::process::id(),
        session_id,
        pane_id,
    );
    if let Some(bin) = ensure_tmux_shim() {
        // PATH prepend is best-effort: interactive shell init files
        // (path_helper, user rc) may reorder it. `LRMUX_TMUX` and
        // `TMUX_BIN` are the reliable handles — absolute paths to the
        // shim binary that survive any shell init.
        let shim = bin.join("tmux").display().to_string();
        env.push(("LRMUX_TMUX".into(), shim.clone()));
        env.push(("TMUX_BIN".into(), shim));
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        if !path.split(':').any(|d| d == bin.to_string_lossy()) {
            env.push(("PATH".into(), format!("{}:{path}", bin.display())));
        }
    }
    env
}

/// Directory holding the `tmux` compatibility shim
/// (`<socket dir>/bin`, created lazily). In tmux-compat mode it is
/// prepended to pane children's PATH so tools that shell out to `tmux`
// reach this binary's tmux-subset command mode (see argv[0] check in
/// `main`). Returns None if the shim could not be installed.
fn ensure_tmux_shim() -> Option<PathBuf> {
    static SHIM_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    SHIM_DIR
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            let sock = crate::ipc::socket_path(crate::server::server_name());
            let dir = sock.parent()?.join("bin");
            std::fs::create_dir_all(&dir).ok()?;
            let link = dir.join("tmux");
            // Refresh the link if it is missing or points elsewhere
            // (e.g. a different lrmux binary after a rebuild).
            let stale = match std::fs::read_link(&link) {
                Ok(target) => target != exe,
                Err(_) => true,
            };
            if stale {
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(&exe, &link).ok()?;
            }
            Some(dir)
        })
        .clone()
}

fn tmux_env_pairs(
    socket: &str,
    server_pid: u32,
    session_id: u32,
    pane_id: u32,
) -> Vec<(String, String)> {
    vec![
        ("TMUX".into(), format!("{socket},{server_pid},{session_id}")),
        ("TMUX_PANE".into(), format!("%{pane_id}")),
        // Real tmux panes also see TERM_PROGRAM=tmux; apps that detect
        // a multiplexer key off it (or off TERM), not just $TMUX.
        ("TERM_PROGRAM".into(), "tmux".into()),
        (
            "TERM_PROGRAM_VERSION".into(),
            crate::client::tmux_shim::TMUX_VERSION.into(),
        ),
    ]
}

/// Open this pane's byte-trace file (`LRMUX_TRACE`), or None when
/// tracing is disabled.
fn open_pane_trace(pane_id: u32) -> Option<std::fs::File> {
    // Namespace by server name: every server shares the same logs dir,
    // so a bare pane-%N.trace would be overwritten by each server.
    let server = crate::server::server_name();
    crate::log::open_trace(&format!("pane-{server}-%{pane_id}.trace"))
}
