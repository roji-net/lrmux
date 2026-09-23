// Client terminal: raw mode, size query. Wraps term::RawModeGuard for the
// controlling terminal (stdin/stdout).

use std::io;
use std::os::fd::AsRawFd;

use crate::term::{self, RawModeGuard};

/// Enter raw mode on stdin. Returns a guard that restores on drop.
pub fn enter_raw_mode() -> io::Result<RawModeGuard> {
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    RawModeGuard::enter(fd)
}

/// Query the current terminal size (rows, cols) from stdout.
pub fn get_size() -> (u16, u16) {
    let stdout = io::stdout();
    let fd = AsRawFd::as_raw_fd(&stdout);
    term::get_size(fd)
}
