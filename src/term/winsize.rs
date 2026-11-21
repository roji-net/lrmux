//! Outer-TTY and PTY window size helpers (TIOCGWINSZ / TIOCSWINSZ).

use std::os::fd::RawFd;

/// Result of `TIOCGWINSZ`: character grid plus optional pixel dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl WinSize {
    pub fn cell_size(&self) -> Option<(u16, u16)> {
        if self.cols == 0 || self.rows == 0 || self.xpixel == 0 || self.ypixel == 0 {
            return None;
        }
        Some((
            (self.xpixel / self.cols).max(1),
            (self.ypixel / self.rows).max(1),
        ))
    }
}

/// Read window size from `fd`. Returns None on ioctl failure.
pub fn get(fd: RawFd) -> Option<WinSize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 {
        return None;
    }
    Some(WinSize {
        rows: ws.ws_row,
        cols: ws.ws_col,
        xpixel: ws.ws_xpixel,
        ypixel: ws.ws_ypixel,
    })
}

/// Best-effort outer TTY size: stdout, then `/dev/tty`.
pub fn outer() -> Option<WinSize> {
    get(libc::STDOUT_FILENO).or_else(|| {
        let fd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if fd < 0 {
            return None;
        }
        let ws = get(fd);
        unsafe {
            libc::close(fd);
        }
        ws
    })
}
