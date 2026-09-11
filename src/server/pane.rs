// Pane: a rectangular region with a PTY running a child process, plus a grid.

use std::io;

use crate::grid::{Cell, Grid};
use crate::pty::{Pty, PtySize, default_shell_argv};
use crate::vt;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

/// A single pane: PTY + grid + VT parser.
pub struct Pane {
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
}

impl Pane {
    /// Spawn a new pane with the default shell.
    pub fn new(rows: u16, cols: u16) -> Self {
        let argv = default_shell_argv();
        let pty = Pty::spawn(&argv, PtySize { rows, cols });
        let grid = Grid::new(rows as usize, cols as usize, 10_000);
        let vt_parser = vte::Parser::new();
        Self {
            pty,
            grid,
            vt_parser,
            rows,
            cols,
            exited: false,
            exit_code: None,
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

    /// Read PTY output, parse into grid. Returns true if the child is still alive.
    pub fn process_pty_output(&mut self) -> io::Result<bool> {
        if self.exited {
            return Ok(false);
        }
        let mut buf = [0u8; 8192];
        let fd = self.pty_fd();
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n > 0 {
            let n = n as usize;
            vt::parse_bytes(&mut self.vt_parser, &mut self.grid, &buf[..n]);
            Ok(true)
        } else if n == 0 {
            Ok(false)
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                Ok(true)
            } else if err.raw_os_error() == Some(libc::EIO) {
                Ok(false)
            } else {
                Err(err)
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
        vt::parse_bytes(&mut self.vt_parser, &mut self.grid, msg.as_bytes());
    }

    /// Check if the pane's child has exited.
    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// Write input bytes to the PTY (keystrokes from client).
    pub fn write_input(&self, data: &[u8]) -> io::Result<()> {
        let fd = self.pty_fd();
        let mut written = 0;
        while written < data.len() {
            let w = unsafe {
                libc::write(
                    fd,
                    data[written..].as_ptr() as *const _,
                    (data.len() - written) as _,
                )
            };
            if w < 0 {
                return Err(io::Error::last_os_error());
            }
            written += w as usize;
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
}
