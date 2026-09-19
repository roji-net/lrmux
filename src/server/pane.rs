// Pane: a rectangular region with a PTY running a child process, plus a grid.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::grid::{Cell, Grid};
use crate::pty::{Pty, PtySize, default_shell_argv};
use crate::vt;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use std::ffi::CString;

/// Global pane ID counter (tmux uses %N format).
static PANE_ID: AtomicU32 = AtomicU32::new(0);

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
}

impl Pane {
    /// Spawn a new pane with the default shell.
    pub fn new(rows: u16, cols: u16) -> Self {
        let argv = default_shell_argv();
        Self::new_with_argv(rows, cols, &argv, None)
    }

    /// Spawn a new pane with the default shell in a specific directory.
    pub fn new_in_cwd(rows: u16, cols: u16, cwd: &str) -> Self {
        let argv = default_shell_argv();
        Self::new_with_argv(rows, cols, &argv, Some(cwd))
    }

    /// Spawn a new pane with a custom command string.
    ///
    /// Runs `$SHELL -ci <command>` so interactive rc files (`.zshrc` /
    /// `.bashrc`) are sourced. Plain `-c` skips them and breaks vim
    /// truecolor / redraw (confirmed: `zsh -c vi` glitches, `zsh -ci vi`
    /// does not). After the command exits the shell exits, so the pane
    /// closes — no `exec` needed. Optional `cwd` sets the child's working
    /// directory (tmux `new-session -c`).
    pub fn new_with_command(rows: u16, cols: u16, command: &str, cwd: Option<&str>) -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let cmd = command.trim();
        let argv = vec![
            CString::new(shell).unwrap(),
            // Combined `-ci` matches the working manual repro (`zsh -ci '…'`).
            // Separate `-i` `-c` should be equivalent; keep the proven form.
            CString::new("-ci").unwrap(),
            CString::new(cmd).unwrap(),
        ];
        Self::new_with_argv(rows, cols, &argv, cwd)
    }

    /// Spawn a new pane with the given argv and optional working directory.
    fn new_with_argv(rows: u16, cols: u16, argv: &[CString], cwd: Option<&str>) -> Self {
        let pty = Pty::spawn(argv, PtySize { rows, cols }, cwd);
        let grid = Grid::new(rows as usize, cols as usize, 10_000);
        let vt_parser = vte::Parser::new();
        Self {
            id: PANE_ID.fetch_add(1, Ordering::Relaxed),
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

    /// Get the PTY master fd (for poll). Returns -1 if the child has exited.
    pub fn pty_fd(&self) -> i32 {
        if self.exited {
            -1
        } else {
            self.pty.master_fd()
        }
    }

    /// Read PTY output, parse into grid. Returns (still_alive, raw_bytes, osc_queries).
    /// raw_bytes is the unprocessed output from the PTY (for control mode forwarding).
    /// osc_queries are OSC 10/11 color probes to proxy to an attached client TTY.
    pub fn process_pty_output(&mut self) -> io::Result<(bool, Vec<u8>, Vec<vt::OscColorQuery>)> {
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
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let n = n as usize;
                raw.extend_from_slice(&buf[..n]);
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
                if !parsed.immediate_replies.is_empty() {
                    // CPR / DSR / DA — answer from our grid state.
                    self.write_input(&parsed.immediate_replies)?;
                }
                osc_queries.extend(parsed.osc_queries);
                if raw.len() >= MAX_DRAIN {
                    return Ok((true, raw, osc_queries));
                }
                continue;
            }
            if n == 0 {
                return Ok((false, raw, osc_queries));
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok((true, raw, osc_queries));
            } else if err.raw_os_error() == Some(libc::EIO) {
                return Ok((false, raw, osc_queries));
            } else {
                return Err(err);
            }
        }
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
}
