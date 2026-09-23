// Pane: a rectangular region with a PTY running a child process, plus a grid.

use std::io;

use crate::grid::{Cell, Grid};
use crate::pty::{Pty, PtySize, default_shell_argv};
use crate::vt;

/// A single pane: PTY + grid + VT parser.
pub struct Pane {
    pub pty: Pty,
    pub grid: Grid,
    pub vt_parser: vte::Parser,
    pub rows: u16,
    pub cols: u16,
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
        }
    }

    /// Get the PTY master fd (for poll).
    pub fn pty_fd(&self) -> i32 {
        self.pty.master_fd()
    }

    /// Read PTY output, parse into grid. Returns true if the child is still alive.
    pub fn process_pty_output(&mut self) -> io::Result<bool> {
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
