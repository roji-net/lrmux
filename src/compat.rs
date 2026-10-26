//! Platform shims for environments with incomplete syscall tables (notably iSH).

use std::time::Duration;

/// Block for approximately `dur`.
///
/// Uses `poll(nfds=0)` instead of `clock_nanosleep` / `nanosleep`. On iSH
/// (i386), syscall 267 (`clock_nanosleep` time32) is unimplemented and
/// `std::thread::sleep` raises SIGSYS — which is how the parent died right
/// after forking a server that kept running.
pub fn sleep(dur: Duration) {
    // Cap to i32::MAX ms; split longer sleeps so a huge Duration still works.
    let mut left = dur.as_millis();
    while left > 0 {
        let chunk = left.min(i32::MAX as u128) as i32;
        // nfds=0: return when timeout elapses (POSIX / Linux / macOS).
        unsafe {
            libc::poll(std::ptr::null_mut(), 0, chunk);
        }
        left -= chunk as u128;
    }
}
