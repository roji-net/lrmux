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

/// Output of one `parse_bytes` pass: local replies + OSC queries to proxy.
#[derive(Debug, Default)]
pub struct ParseResult {
    /// CPR / DSR / Primary DA — written straight back into the PTY.
    pub immediate_replies: Vec<u8>,
    /// OSC 10/11 — forwarded to an attached client for a real TTY answer.
    pub osc_queries: Vec<OscColorQuery>,
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
        self.result.osc_queries.push(OscColorQuery {
            code,
            bell_terminated,
        });
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

            // SGR - set graphic rendition
            'm' => {
                self.grid.set_sgr(&p);
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
                            _ => {}
                        }
                    }
                }
            }

            // SCP - save cursor position
            's' => {
                self.grid.save_cursor();
            }
            // RCP - restore cursor position
            'u' => {
                self.grid.restore_cursor();
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
                            self.result.immediate_replies.extend_from_slice(b"\x1b[0n");
                        }
                        6 => {
                            // CPR - cursor position (1-based)
                            let row = self.grid.cursor_row + 1;
                            let col = self.grid.cursor_col + 1;
                            self.result
                                .immediate_replies
                                .extend_from_slice(format!("\x1b[{row};{col}R").as_bytes());
                        }
                        _ => {}
                    }
                }
            }

            // Primary DA - CSI c / CSI 0 c
            'c' if intermediates.is_empty() => {
                // Claim VT220-ish capabilities (same class as xterm defaults).
                self.result
                    .immediate_replies
                    .extend_from_slice(b"\x1b[?62;1;2c");
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
    use crate::grid::Grid;

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
    fn osc_color_query_proxied_cpr_local() {
        let mut grid = Grid::new(24, 80, 100);
        grid.move_cursor(3, 7);
        let mut parser = vte::Parser::new();
        let result = parse_bytes(
            &mut parser,
            &mut grid,
            b"\x1b]11;?\x07\x1b]10;?\x07\x1b[6n\x1b[5n",
        );
        assert_eq!(result.osc_queries.len(), 2);
        assert_eq!(result.osc_queries[0].code, 11);
        assert_eq!(result.osc_queries[1].code, 10);
        let s = String::from_utf8_lossy(&result.immediate_replies);
        assert!(s.contains("\x1b[4;8R"), "missing CPR reply: {s:?}");
        assert!(s.contains("\x1b[0n"), "missing DSR status reply: {s:?}");
        assert!(
            !s.contains("rgb:"),
            "must not invent OSC color replies: {s:?}"
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
}
