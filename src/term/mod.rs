// Terminal: termios raw mode, terminal size query, escape-sequence writer.

use std::os::fd::BorrowedFd;

use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};

/// Guard that restores the original terminal settings when dropped.
pub struct RawModeGuard {
    fd: std::os::fd::RawFd,
    original: Termios,
}

impl RawModeGuard {
    /// Enter raw mode on the given fd. Returns a guard that restores
    /// the original settings on drop.
    pub fn enter(fd: std::os::fd::RawFd) -> std::io::Result<Self> {
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let original = tcgetattr(borrowed).map_err(std::io::Error::from)?;

        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(borrowed, SetArg::TCSANOW, &raw).map_err(std::io::Error::from)?;

        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = tcsetattr(borrowed, SetArg::TCSANOW, &self.original);
    }
}

/// Query the terminal size (rows, cols) via ioctl(TIOCGWINSZ).
pub fn get_size(fd: std::os::fd::RawFd) -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) < 0 {
            return (24, 80);
        }
    }
    (ws.ws_row, ws.ws_col)
}

/// Write helpers for escape sequences to stdout.
pub mod escapes;
