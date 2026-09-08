// Escape sequences: cursor positioning, clear, SGR with truecolor,
// alternate screen, scroll regions.

pub fn move_cursor(row: u16, col: u16) -> String {
    format!("\x1b[{};{}H", row, col)
}

pub fn clear_screen() -> &'static str {
    "\x1b[2J"
}

pub fn sgr_truecolor_fg(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

pub fn sgr_truecolor_bg(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[48;2;{};{};{}m", r, g, b)
}

pub fn reset_sgr() -> &'static str {
    "\x1b[0m"
}

pub fn enter_alt_screen() -> &'static str {
    "\x1b[?1049h"
}

pub fn exit_alt_screen() -> &'static str {
    "\x1b[?1049l"
}
