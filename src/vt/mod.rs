// VT parser: wraps the vte crate to parse child PTY output into grid updates.
//
// Implements vte::Perform, dispatching escape sequences to Grid methods.

use vte::{Params, Perform};

use crate::grid::Grid;

/// OSC 10/11 color query from the child — must be answered by the real
/// client TTY (not invented by the server).
#[derive(Debug, Clone)]
pub struct OscColorQuery {
    /// 10 = foreground, 11 = background.
    pub code: u8,
    pub bell_terminated: bool,
}

/// One reply-able item from a `parse_bytes` pass.
#[derive(Debug)]
pub enum PendingReply {
    /// CPR / DSR / Primary DA — fully synthesized bytes.
    Bytes(Vec<u8>),
    /// OSC 10/11 — needs the outer palette (cached or proxied to a client).
    Osc(OscColorQuery),
}

/// Output of one `parse_bytes` pass: replies to the child's terminal
/// queries **in the order the child emitted them**. Order matters:
/// probing libraries like terminal-colorsaurus treat a DA-first reply as
/// proof that the terminal skipped the color query (they write
/// `OSC 10;? OSC 11;? DA1` and read replies sequentially — the same bug
/// that got GNU Screen blacklisted).
#[derive(Debug, Default)]
pub struct ParseResult {
    pub replies: Vec<PendingReply>,
}

/// A VT terminal handler that updates a Grid as it parses escape sequences.
pub struct VtHandler<'a> {
    pub grid: &'a mut Grid,
    pub result: &'a mut ParseResult,
}

impl Perform for VtHandler<'_> {
    fn print(&mut self, c: char) {
        self.grid.print(c);
    }

    fn execute(&mut self, byte: u8) {
        // C0 control characters
        match byte {
            0x08 => self.grid.backspace(),        // BS
            0x09 => self.grid.tab(),              // HT (tab)
            0x0A..=0x0C => self.grid.line_feed(), // LF, VT, FF
            0x0D => self.grid.carriage_return(),  // CR
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        // Color queries (`OSC 10/11 ; ?`) must be answered by the outer
        // terminal. Inventing rgb:0000/0000/0000 lies about the real palette
        // and can make colorschemes look wrong. CPR/DSR stay local.
        if params.len() < 2 || params[1] != b"?" {
            return;
        }
        let code = match params[0] {
            b"10" => 10u8,
            b"11" => 11u8,
            _ => return,
        };
        self.result.replies.push(PendingReply::Osc(OscColorQuery {
            code,
            bell_terminated,
        }));
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Collect params into a flat Vec for easier handling.
        let p: Vec<u16> = params.iter().flat_map(|sub| sub.iter().copied()).collect();

        // Helper to get param or default.
        let param = |idx: usize, default: u16| -> u16 { p.get(idx).copied().unwrap_or(default) };
        // Helper for cursor movement / scroll commands where 0 means 1 (per VT spec).
        let param1 = |idx: usize| -> u16 {
            let v = p.get(idx).copied().unwrap_or(1);
            if v == 0 { 1 } else { v }
        };

        match action {
            // Cursor positioning
            'A' => {
                // CUU - cursor up
                let n = param1(0) as i32;
                self.grid.move_cursor_rel(-n, 0);
            }
            'B' => {
                // CUD - cursor down
                let n = param1(0) as i32;
                self.grid.move_cursor_rel(n, 0);
            }
            'C' => {
                // CUF - cursor forward
                let n = param1(0) as i32;
                self.grid.move_cursor_rel(0, n);
            }
            'D' => {
                // CUB - cursor back
                let n = param1(0) as i32;
                self.grid.move_cursor_rel(0, -n);
            }
            'E' => {
                // CNL - cursor next line
                let n = param1(0);
                self.grid.move_cursor(self.grid.cursor_row + n as usize, 0);
            }
            'F' => {
                // CPL - cursor previous line
                let n = param1(0) as usize;
                let row = self.grid.cursor_row.saturating_sub(n);
                self.grid.move_cursor(row, 0);
            }
            'G' => {
                // CHA - cursor horizontal absolute
                let col = param(0, 1) as usize;
                self.grid
                    .move_cursor(self.grid.cursor_row, col.saturating_sub(1));
            }
            'H' | 'f' => {
                // CUP / HVP - cursor position
                let row = param(0, 1) as usize;
                let col = param(1, 1) as usize;
                self.grid
                    .move_cursor(row.saturating_sub(1), col.saturating_sub(1));
            }
            'd' => {
                // VPA - vertical position absolute
                let row = param(0, 1) as usize;
                self.grid
                    .move_cursor(row.saturating_sub(1), self.grid.cursor_col);
            }

            // Erase
            'J' => {
                // ED - erase in display
                match param(0, 0) {
                    0 => self.grid.erase_to_end_of_screen(),
                    1 => self.grid.erase_to_start_of_screen(),
                    2 | 3 => self.grid.erase_screen(),
                    _ => {}
                }
            }
            'K' => {
                // EL - erase in line
                match param(0, 0) {
                    0 => self.grid.erase_to_end_of_line(),
                    1 => self.grid.erase_to_cursor(),
                    2 => self.grid.erase_line(),
                    _ => {}
                }
            }

            // SGR - set graphic rendition.
            // Only plain CSI ... m is SGR. CSI > Ps ; Pv m is xterm's
            // modifyOtherKeys (Pp=4); treating it as SGR makes param 4
            // stick underline on forever — Claude/Ink enable it often,
            // which is why whole UIs look underlined inside lrmux but not
            // in a real terminal.
            'm' if intermediates.is_empty() => {
                let groups: Vec<&[u16]> = params.iter().collect();
                self.grid.set_sgr(&groups);
            }

            // Scroll
            'S' => {
                // SU - scroll up
                let n = param1(0) as usize;
                self.grid.scroll_up(n);
            }
            'T' => {
                // SD - scroll down
                let n = param1(0) as usize;
                self.grid.scroll_down(n);
            }
            'L' => {
                // IL - insert lines at cursor
                let n = param1(0) as usize;
                self.grid.insert_lines(n);
            }
            'M' => {
                // DL - delete lines at cursor
                let n = param1(0) as usize;
                self.grid.delete_lines(n);
            }
            '@' => {
                // ICH - insert characters at cursor
                let n = param1(0) as usize;
                self.grid.insert_chars(n);
            }
            'P' => {
                // DCH - delete characters at cursor
                let n = param1(0) as usize;
                self.grid.delete_chars(n);
            }
            'X' => {
                // ECH - erase characters at cursor (no shift)
                let n = param1(0) as usize;
                self.grid.erase_chars(n);
            }

            // DECSTBM - set scroll region
            'r' => {
                let top = param(0, 1) as usize;
                let bottom = param(1, 0) as usize;
                if bottom == 0 {
                    self.grid.reset_scroll_region();
                } else {
                    self.grid.set_scroll_region(top.saturating_sub(1), bottom);
                }
                // Move cursor to top-left of scroll region.
                self.grid.move_cursor(top.saturating_sub(1), 0);
            }

            // Cursor visibility and private modes
            'h' => {
                // SM - set mode (e.g., ?25 = show cursor, ?1 = app cursor keys)
                if intermediates.contains(&b'?') {
                    for m in &p {
                        match *m {
                            1 => self.grid.app_cursor_keys = true,
                            25 => self.grid.cursor_visible = true,
                            1000 => self.grid.mouse_tracking |= 1,
                            1002 => self.grid.mouse_tracking |= 2,
                            1003 => self.grid.mouse_tracking |= 4,
                            1005 => self.grid.mouse_fmt = 5,
                            1006 => self.grid.mouse_fmt = 6,
                            1007 => self.grid.mouse_altscroll = true,
                            _ => {}
                        }
                    }
                }
            }
            'l' => {
                // RM - reset mode (e.g., ?25 = hide cursor, ?1 = normal cursor keys)
                if intermediates.contains(&b'?') {
                    for m in &p {
                        match *m {
                            1 => self.grid.app_cursor_keys = false,
                            25 => self.grid.cursor_visible = false,
                            1000 => self.grid.mouse_tracking &= !1,
                            1002 => self.grid.mouse_tracking &= !2,
                            1003 => self.grid.mouse_tracking &= !4,
                            1005 | 1006 => self.grid.mouse_fmt = 0,
                            1007 => self.grid.mouse_altscroll = false,
                            _ => {}
                        }
                    }
                }
            }

            // SCP / RCP — plain forms only. `\x1b[?u` is a kitty keyboard
            // flags query and `\x1b[>Nu` / `\x1b[<u` push/pop its state —
            // all share the 'u' final byte. Treating them as RCP jumps
            // the cursor to the saved position and corrupts every later
            // relative move (observed: Claude's diff renderer drawing at
            // the top of the screen).
            's' if intermediates.is_empty() => {
                self.grid.save_cursor();
            }
            'u' if intermediates.is_empty() => {
                self.grid.restore_cursor();
            }
            // kitty keyboard flags query — answer 0 (no progressive
            // enhancements) so the child falls back to legacy keys.
            'u' if intermediates == b"?" => {
                self.result
                    .replies
                    .push(PendingReply::Bytes(b"\x1b[?0u".to_vec()));
            }

            // Repeat preceding character (REP)
            'b' => {
                let n = param(0, 1) as usize;
                if n > 0
                    && self.grid.cursor_col > 0
                    && let Some(cell) = self
                        .grid
                        .cell(self.grid.cursor_row, self.grid.cursor_col - 1)
                {
                    let ch = cell.ch;
                    for _ in 0..n {
                        self.grid.print(ch);
                    }
                }
            }

            // DSR - device status report (CSI 5 n / CSI 6 n)
            'n' => {
                if intermediates.is_empty() {
                    match param(0, 0) {
                        5 => {
                            // Status: terminal OK
                            self.result
                                .replies
                                .push(PendingReply::Bytes(b"\x1b[0n".to_vec()));
                        }
                        6 => {
                            // CPR - cursor position (1-based)
                            let row = self.grid.cursor_row + 1;
                            let col = self.grid.cursor_col + 1;
                            self.result.replies.push(PendingReply::Bytes(
                                format!("\x1b[{row};{col}R").into_bytes(),
                            ));
                        }
                        _ => {}
                    }
                }
            }

            // Primary DA - CSI c / CSI 0 c
            'c' if intermediates.is_empty() => {
                // Claim VT220-ish capabilities (same class as xterm defaults).
                self.result
                    .replies
                    .push(PendingReply::Bytes(b"\x1b[?62;1;2c".to_vec()));
            }

            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        // Handle ESC sequences (7-bit)
        match byte {
            b'7' => self.grid.save_cursor(),    // DECSC
            b'8' => self.grid.restore_cursor(), // DECRC
            b'M' => {
                // RI - reverse line feed. Scroll down when at the top of the
                // scroll region (not merely row 0 — DECSTBM may raise it).
                if self.grid.cursor_row <= self.grid.scroll_top {
                    self.grid.scroll_down(1);
                } else {
                    self.grid.move_cursor_rel(-1, 0);
                }
            }
            b'D' => self.grid.line_feed(), // IND - index (line feed)
            b'E' => {
                // NEL - next line
                self.grid.carriage_return();
                self.grid.line_feed();
            }
            _ => {}
        }
    }
}

/// Parse a buffer of bytes through the VT parser, updating the grid.
/// Returns local replies (CPR/DSR) and any OSC color queries to proxy.
pub fn parse_bytes(parser: &mut vte::Parser, grid: &mut Grid, bytes: &[u8]) -> ParseResult {
    let mut result = ParseResult::default();
    let mut handler = VtHandler {
        grid,
        result: &mut result,
    };
    parser.advance(&mut handler, bytes);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Color, Grid};

    fn row_text(grid: &Grid, row: usize) -> String {
        grid.row(row)
            .unwrap()
            .iter()
            .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn osc_query_forms_detected() {
        let cases: &[(&[u8], u8)] = &[
            (b"\x1b]11;?\x07", 11),
            (b"\x1b]10;?\x07", 10),
            (b"\x1b]11;?\x1b\\", 11),
            (b"\x1b]10;?\x1b\\", 10),
            // some terminals send with empty middle
            (b"\x1b]11;?\x07\x1b]10;?\x07", 11),
        ];
        for (bytes, code) in cases {
            let mut grid = Grid::new(4, 40, 10);
            let mut parser = vte::Parser::new();
            let result = parse_bytes(&mut parser, &mut grid, bytes);
            assert!(
                result
                    .replies
                    .iter()
                    .any(|r| matches!(r, PendingReply::Osc(q) if q.code == *code)),
                "failed to detect OSC {code} in {bytes:?}, got {:?}",
                result.replies
            );
        }
    }

    #[test]
    fn osc_color_query_proxied_cpr_local() {
        let mut grid = Grid::new(24, 80, 100);
        grid.move_cursor(3, 7);
        let mut parser = vte::Parser::new();
        let result = parse_bytes(
            &mut parser,
            &mut grid,
            b"\x1b]11;?\x07\x1b]10;?\x07\x1b[6n\x1b[5n",
        );
        assert_eq!(result.replies.len(), 4);
        // Order preserved: the two OSC queries came before CPR and DSR.
        assert!(
            matches!(&result.replies[0], PendingReply::Osc(q) if q.code == 11),
            "replies: {:?}",
            result.replies
        );
        assert!(
            matches!(&result.replies[1], PendingReply::Osc(q) if q.code == 10),
            "replies: {:?}",
            result.replies
        );
        assert!(
            matches!(&result.replies[2], PendingReply::Bytes(b) if b == b"\x1b[4;8R"),
            "missing CPR reply: {:?}",
            result.replies
        );
        assert!(
            matches!(&result.replies[3], PendingReply::Bytes(b) if b == b"\x1b[0n"),
            "missing DSR status reply: {:?}",
            result.replies
        );
    }

    #[test]
    fn csi_dch_deletes_at_cursor() {
        let mut grid = Grid::new(1, 10, 100);
        let mut parser = vte::Parser::new();
        parse_bytes(&mut parser, &mut grid, b"hello\x1b[1G\x1b[P");
        assert_eq!(row_text(&grid, 0), "ello");
        assert_eq!(grid.cursor_col, 0);
    }

    #[test]
    fn csi_ich_inserts_at_cursor() {
        let mut grid = Grid::new(1, 10, 100);
        let mut parser = vte::Parser::new();
        parse_bytes(&mut parser, &mut grid, b"ello\x1b[1G\x1b[@h");
        assert_eq!(row_text(&grid, 0), "hello");
    }

    #[test]
    fn modify_other_keys_csi_is_not_sgr_underline() {
        // xterm CSI > 4 ; 2 m sets modifyOtherKeys — must NOT enable underline.
        let mut grid = Grid::new(1, 20, 10);
        let mut parser = vte::Parser::new();
        parse_bytes(&mut parser, &mut grid, b"\x1b[>4;2mhello\x1b[>4m!");
        assert!(!grid.attrs.underline, "pen must not stick underline");
        let row = grid.row(0).unwrap();
        for cell in row.iter().take(6) {
            assert!(
                !cell.attrs.underline,
                "cell {:?} must not be underlined",
                cell.ch
            );
        }
        // Real SGR underline still works.
        parse_bytes(&mut parser, &mut grid, b"\x1b[4mU\x1b[24m");
        let u = grid.row(0).unwrap()[6].attrs.underline;
        assert!(u, "plain CSI 4 m must still set underline");
    }

    #[test]
    fn kitty_keyboard_query_does_not_restore_cursor() {
        // Claude emits \x1b[?u (kitty keyboard flags query) at startup.
        // Its final byte is 'u' — treating it as RCP jumps the cursor to
        // the saved position and corrupts every subsequent relative move.
        let mut grid = Grid::new(10, 20, 10);
        grid.move_cursor(8, 5);
        grid.save_cursor();
        grid.move_cursor(3, 7);
        let mut parser = vte::Parser::new();
        let r = parse_bytes(&mut parser, &mut grid, b"\x1b[?u");
        assert_eq!(
            (grid.cursor_row, grid.cursor_col),
            (3, 7),
            "\x1b[?u must not move the cursor"
        );
        // The query gets an answer: 0 = no progressive enhancements.
        assert!(
            r.replies
                .iter()
                .any(|x| matches!(x, PendingReply::Bytes(b) if b == b"\x1b[?0u")),
            "kitty flags query should be answered: {:?}",
            r.replies
        );

        // Push/pop forms (\x1b[>1u / \x1b[<u) are ignored, not cursor ops.
        parse_bytes(&mut parser, &mut grid, b"\x1b[>1u\x1b[<u");
        assert_eq!((grid.cursor_row, grid.cursor_col), (3, 7));

        // Plain \x1b[u still restores.
        parse_bytes(&mut parser, &mut grid, b"\x1b[u");
        assert_eq!((grid.cursor_row, grid.cursor_col), (8, 5));
    }

    #[test]
    fn decset_mouse_modes() {
        let mut grid = Grid::new(1, 20, 10);
        let mut parser = vte::Parser::new();
        parse_bytes(&mut parser, &mut grid, b"\x1b[?1002h\x1b[?1006h\x1b[?1007h");
        assert_eq!(grid.mouse_tracking, 2);
        assert_eq!(grid.mouse_fmt, 6);
        assert!(grid.mouse_altscroll);
        assert!(grid.wants_mouse());
        parse_bytes(&mut parser, &mut grid, b"\x1b[?1002l\x1b[?1006l");
        assert_eq!(grid.mouse_tracking, 0);
        assert_eq!(grid.mouse_fmt, 0);
        assert!(!grid.wants_mouse());
        assert!(grid.mouse_altscroll); // 1007 untouched
    }
    #[test]
    fn sgr_combined_fg_bg_extended_colors() {
        // Rich/Textual emit fg+bg in ONE SGR: \x1b[38;2;f;f;f;48;2;b;b;bm.
        // The fg args must not eat the 48 group, and leftover RGB
        // components must never land on SGR 0 (reset) — a regression here
        // made styled text render with default colors.
        let mut grid = Grid::new(4, 80, 10);
        let mut parser = vte::Parser::new();
        parse_bytes(
            &mut parser,
            &mut grid,
            b"\x1b[38;2;255;255;255;48;2;0;0;255mHELLO",
        );
        let c = &grid.row(0).unwrap()[0];
        assert_eq!(c.fg, Color::Rgb(255, 255, 255));
        assert_eq!(c.bg, Color::Rgb(0, 0, 255));

        // Indexed + trailing attrs in the same sequence.
        let mut grid = Grid::new(4, 80, 10);
        let mut parser = vte::Parser::new();
        parse_bytes(&mut parser, &mut grid, b"\x1b[38;5;196;1;48;5;21mX");
        let c = &grid.row(0).unwrap()[0];
        assert_eq!(c.fg, Color::Indexed(196));
        assert_eq!(c.bg, Color::Indexed(21));
        assert!(c.attrs.bold);

        // Colon subparams and colorspace-prefixed truecolor.
        let mut grid = Grid::new(4, 80, 10);
        let mut parser = vte::Parser::new();
        parse_bytes(
            &mut parser,
            &mut grid,
            b"\x1b[38:2:10:20:30;48:2:0:40:50:60mY",
        );
        let c = &grid.row(0).unwrap()[0];
        assert_eq!(c.fg, Color::Rgb(10, 20, 30));
        assert_eq!(c.bg, Color::Rgb(40, 50, 60));
    }
}
