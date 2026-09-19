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

/// Parse an OSC 10/11 color reply from a real TTY.
///
/// Accepts `OSC 10 ; rgb:RRRR/GGGG/BBBB ST` (and OSC 11), plus `#RRGGBB`.
/// Returns `(code, (r, g, b))` where `code` is 10 or 11.
pub fn parse_osc_color_reply(data: &[u8]) -> Option<(u8, (u8, u8, u8))> {
    let s = std::str::from_utf8(data).ok()?;
    let start = s.find("\x1b]")?;
    let body = &s[start + 2..];
    let end = body
        .find('\x07')
        .or_else(|| body.find("\x1b\\"))
        .unwrap_or(body.len());
    let body = &body[..end];
    let (code_str, rest) = body.split_once(';')?;
    let code: u8 = code_str.parse().ok()?;
    if code != 10 && code != 11 {
        return None;
    }
    let rgb = parse_color_spec(rest.trim())?;
    Some((code, rgb))
}

fn parse_color_spec(spec: &str) -> Option<(u8, u8, u8)> {
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some((r, g, b));
        }
        return None;
    }
    let rest = spec.strip_prefix("rgb:")?;
    let mut parts = rest.split('/');
    let r = parse_osc_channel(parts.next()?)?;
    let g = parse_osc_channel(parts.next()?)?;
    let b = parse_osc_channel(parts.next()?)?;
    Some((r, g, b))
}

/// xterm sends 1–4 hex digits per channel; take the high 8 bits of the 16-bit value.
fn parse_osc_channel(s: &str) -> Option<u8> {
    if s.is_empty() || s.len() > 4 {
        return None;
    }
    let v = u16::from_str_radix(s, 16).ok()?;
    let shifted = match s.len() {
        1 => v << 12,
        2 => v << 8,
        3 => v << 4,
        _ => v,
    };
    Some((shifted >> 8) as u8)
}

#[cfg(test)]
mod osc_tests {
    use super::*;

    #[test]
    fn parse_rgb_bell() {
        let d = b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07";
        assert_eq!(parse_osc_color_reply(d), Some((11, (0x1e, 0x1e, 0x2e))));
    }

    #[test]
    fn parse_hash_st() {
        let d = b"\x1b]10;#cccccc\x1b\\";
        assert_eq!(parse_osc_color_reply(d), Some((10, (0xcc, 0xcc, 0xcc))));
    }
}
