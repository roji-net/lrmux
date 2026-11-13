//! Platform shims for environments with incomplete syscall tables (notably iSH).

use std::ffi::CStr;
use std::time::Duration;

/// Short hostname for this machine (`HOSTNAME` env, else `gethostname`).
/// Drops a trailing domain so `host.local` → `host`.
pub fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        let h = h.trim();
        if !h.is_empty() {
            return short_host(h);
        }
    }
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0
        && let Ok(c) = CStr::from_bytes_until_nul(&buf)
        && let Ok(s) = c.to_str()
    {
        let s = s.trim();
        if !s.is_empty() {
            return short_host(s);
        }
    }
    "local".to_string()
}

fn short_host(h: &str) -> String {
    h.split('.').next().unwrap_or(h).to_string()
}

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
