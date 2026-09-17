// Terminal: termios raw mode, terminal size query, escape-sequence writer.

use std::os::fd::BorrowedFd;

use nix::sys::termios::{
    InputFlags, OutputFlags, SetArg, SpecialCharacterIndices, Termios, cfmakeraw, tcgetattr,
    tcsetattr,
};

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

/// Guard for tmux control mode. Reproduces the exact termios a real
/// `tmux -CC` leaves on the PTY (verified with stty): essentially
/// cfmakeraw, but with ICRNL kept (input CR→NL) and OPOST|ONLCR kept
/// (output NL→CRNL). All control characters are NUL'd out so nothing —
/// ^C, ^T (SIGINFO), ^Y (DSUSP), ^\ — can generate signals or be
/// swallowed by the line discipline.
pub struct ControlModeGuard {
    fd: std::os::fd::RawFd,
    original: Termios,
}

impl ControlModeGuard {
    pub fn enter(fd: std::os::fd::RawFd) -> std::io::Result<Self> {
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let original = tcgetattr(borrowed).map_err(std::io::Error::from)?;

        let mut mode = original.clone();
        cfmakeraw(&mut mode);
        // Re-enable what tmux keeps: CR→NL on input, NL→CRNL on output.
        mode.input_flags |= InputFlags::ICRNL;
        mode.output_flags |= OutputFlags::OPOST | OutputFlags::ONLCR;
        // tmux clears these beyond cfmakeraw (verified on macOS).
        mode.input_flags &= !(InputFlags::IMAXBEL | InputFlags::IUTF8);
        // NUL out every control character (matches tmux: all c_cc = 0),
        // then restore VMIN=1/VTIME=0 so read() returns per byte.
        for c in mode.control_chars.iter_mut() {
            *c = 0;
        }
        mode.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        mode.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        tcsetattr(borrowed, SetArg::TCSANOW, &mode).map_err(std::io::Error::from)?;

        Ok(Self { fd, original })
    }
}

impl Drop for ControlModeGuard {
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
    let rows = if ws.ws_row == 0 { 24 } else { ws.ws_row };
    let cols = if ws.ws_col == 0 { 80 } else { ws.ws_col };
    (rows, cols)
}

/// Write helpers for escape sequences to stdout.
pub mod escapes;
