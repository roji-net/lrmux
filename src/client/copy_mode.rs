// Copy/scrollback mode: vi-style navigation of scrollback history.
//
// Entered via Prefix [ (or F7 in future). The client stops relaying input
// to the server and instead navigates the scrollback + visible grid with
// vi-style keys. Text can be selected and copied to the system clipboard.
//
// Virtual row coordinate system:
//   0 .. scrollback_len-1           = scrollback rows (oldest to newest)
//   scrollback_len .. total_rows-1  = visible grid rows (top to bottom)

use std::io::{self, Write};

use crate::grid::{Cell, Color, Grid};

/// Result of processing a key in copy mode.
pub enum CopyAction {
    /// Stay in copy mode, re-render.
    Continue,
    /// Exit copy mode and resume normal operation.
    Quit,
    /// Copy the selected text to clipboard, then quit.
    Copy(String),
}

/// Copy mode state: cursor position, selection, viewport.
pub struct CopyMode {
    /// Virtual row (0 = oldest scrollback row).
    pub vrow: usize,
    /// Column.
    pub vcol: usize,
    /// Selection start position (vrow, vcol). None if no selection.
    pub selection_start: Option<(usize, usize)>,
    /// Top virtual row visible in the viewport.
    pub viewport_top: usize,
}

impl CopyMode {
    /// Enter copy mode. Cursor starts at the current grid cursor position.
    pub fn new(scrollback_len: usize, grid_cursor_row: usize, grid_cursor_col: usize) -> Self {
        let vrow = scrollback_len + grid_cursor_row;
        let vcol = grid_cursor_col;
        Self {
            vrow,
            vcol,
            selection_start: None,
            viewport_top: scrollback_len, // Start showing the visible grid.
        }
    }

    /// Total number of virtual rows (scrollback + visible grid).
    fn total_rows(grid: &Grid) -> usize {
        grid.scrollback.len() + grid.rows()
    }

    /// Move the cursor by a delta. Adjusts the viewport to follow.
    fn move_cursor(&mut self, dvrow: i32, dvcol: i32, grid: &Grid) {
        let total = Self::total_rows(grid);
        let cols = grid.cols();
        self.vrow = (self.vrow as i32 + dvrow).max(0).min(total as i32 - 1) as usize;
        self.vcol = (self.vcol as i32 + dvcol).max(0).min(cols as i32 - 1) as usize;
    }

    /// Move the cursor to an absolute virtual row. Adjusts viewport.
    fn goto_row(&mut self, vrow: usize, grid: &Grid) {
        let total = Self::total_rows(grid);
        self.vrow = vrow.min(total.saturating_sub(1));
    }

    /// Scroll the viewport so the cursor is visible.
    fn ensure_cursor_visible(&mut self, view_rows: usize) {
        if self.vrow < self.viewport_top {
            self.viewport_top = self.vrow;
        } else if self.vrow >= self.viewport_top + view_rows {
            self.viewport_top = self.vrow.saturating_sub(view_rows - 1);
        }
    }

    /// Get a row of cells by virtual row index.
    fn get_vrow<'a>(&self, grid: &'a Grid, vrow: usize) -> Option<&'a [Cell]> {
        let sb_len = grid.scrollback.len();
        if vrow < sb_len {
            grid.scrollback.get(vrow)
        } else {
            grid.row(vrow - sb_len)
        }
    }

    /// Find the first non-blank column in a row (for `^`).
    fn first_non_blank(&self, grid: &Grid, vrow: usize) -> usize {
        if let Some(row) = self.get_vrow(grid, vrow) {
            for (i, cell) in row.iter().enumerate() {
                if cell.ch != '\0' && cell.ch != ' ' {
                    return i;
                }
            }
        }
        0
    }

    /// Find the last non-blank column in a row (for `$`).
    fn last_non_blank(&self, grid: &Grid, vrow: usize) -> usize {
        let cols = grid.cols();
        if let Some(row) = self.get_vrow(grid, vrow) {
            for i in (0..row.len()).rev() {
                let ch = row[i].ch;
                if ch != '\0' && ch != ' ' {
                    return i.min(cols.saturating_sub(1));
                }
            }
        }
        cols.saturating_sub(1)
    }

    /// Process a single input byte in copy mode.
    /// Returns the action to take.
    pub fn process_key(&mut self, byte: u8, grid: &Grid, view_rows: usize) -> CopyAction {
        match byte {
            // Quit copy mode.
            b'q' | 0x1b => CopyAction::Quit, // q or Esc

            // Movement: h/j/k/l (vi-style).
            b'h' => {
                self.move_cursor(0, -1, grid);
                CopyAction::Continue
            }
            b'l' => {
                self.move_cursor(0, 1, grid);
                CopyAction::Continue
            }
            b'j' => {
                self.move_cursor(1, 0, grid);
                self.ensure_cursor_visible(view_rows);
                CopyAction::Continue
            }
            b'k' => {
                self.move_cursor(-1, 0, grid);
                self.ensure_cursor_visible(view_rows);
                CopyAction::Continue
            }

            // Start/end of line.
            b'0' => {
                self.vcol = 0;
                CopyAction::Continue
            }
            b'^' => {
                self.vcol = self.first_non_blank(grid, self.vrow);
                CopyAction::Continue
            }
            b'$' => {
                self.vcol = self.last_non_blank(grid, self.vrow);
                CopyAction::Continue
            }

            // Top / bottom of scrollback.
            b'g' => {
                self.goto_row(0, grid);
                self.ensure_cursor_visible(view_rows);
                CopyAction::Continue
            }
            b'G' => {
                let total = Self::total_rows(grid);
                self.goto_row(total.saturating_sub(1), grid);
                self.vcol = self.first_non_blank(grid, self.vrow);
                self.ensure_cursor_visible(view_rows);
                CopyAction::Continue
            }

            // Half-page up / down.
            0x15 => {
                // Ctrl-u
                let half = (view_rows / 2).max(1);
                self.move_cursor(-(half as i32), 0, grid);
                self.ensure_cursor_visible(view_rows);
                CopyAction::Continue
            }
            0x04 => {
                // Ctrl-d
                let half = (view_rows / 2).max(1);
                self.move_cursor(half as i32, 0, grid);
                self.ensure_cursor_visible(view_rows);
                CopyAction::Continue
            }

            // Full page up / down.
            0x02 => {
                // Ctrl-b
                let page = view_rows.max(1);
                self.move_cursor(-(page as i32), 0, grid);
                self.ensure_cursor_visible(view_rows);
                CopyAction::Continue
            }
            0x06 => {
                // Ctrl-f
                let page = view_rows.max(1);
                self.move_cursor(page as i32, 0, grid);
                self.ensure_cursor_visible(view_rows);
                CopyAction::Continue
            }

            // Begin/end selection (Space toggles).
            b' ' => {
                if self.selection_start.is_some() {
                    // End selection: copy and quit.
                    if let Some(text) = self.get_selected_text(grid) {
                        CopyAction::Copy(text)
                    } else {
                        self.selection_start = None;
                        CopyAction::Continue
                    }
                } else {
                    // Begin selection.
                    self.selection_start = Some((self.vrow, self.vcol));
                    CopyAction::Continue
                }
            }

            // Enter: copy selection (if any) and quit.
            b'\r' | b'\n' => {
                if let Some(text) = self.get_selected_text(grid) {
                    CopyAction::Copy(text)
                } else {
                    CopyAction::Quit
                }
            }

            _ => CopyAction::Continue, // Unknown key — ignore.
        }
    }

    /// Get the selected text as a string. Returns None if no selection.
    fn get_selected_text(&self, grid: &Grid) -> Option<String> {
        let start = self.selection_start?;
        let end = (self.vrow, self.vcol);

        // Normalize: start should be before end.
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };

        let (sr, sc) = start;
        let (er, ec) = end;

        let mut text = String::new();
        for vrow in sr..=er {
            if let Some(row) = self.get_vrow(grid, vrow) {
                let col_start = if vrow == sr { sc } else { 0 };
                let col_end = if vrow == er { ec + 1 } else { row.len() };
                for cell in &row[col_start..col_end.min(row.len())] {
                    let ch = cell.ch;
                    if ch == '\0' {
                        text.push(' ');
                    } else {
                        text.push(ch);
                    }
                }
                if vrow < er {
                    text.push('\n');
                }
            }
        }
        if text.is_empty() { None } else { Some(text) }
    }

    /// Render the copy mode view: scrollback + visible grid with cursor and selection.
    pub fn render(
        &self,
        stdout: &mut io::Stdout,
        grid: &Grid,
        view_rows: usize,
        term_cols: usize,
        status_text: &str,
    ) -> io::Result<()> {
        let cols = grid.cols().min(term_cols);

        // Clear screen.
        stdout.write_all(b"\x1b[2J\x1b[H")?;

        // Render each visible row.
        // Track SGR state to avoid emitting it for every cell.
        let mut cur_fg: Option<Color> = None;
        let mut cur_bg: Option<Color> = None;
        let mut cur_attrs: Option<crate::grid::Attr> = None;
        let mut cur_selected = false;

        for screen_row in 0..view_rows {
            let vrow = self.viewport_top + screen_row;
            if vrow >= Self::total_rows(grid) {
                break;
            }

            // Position cursor at the start of this row.
            write!(stdout, "\x1b[{};1H", screen_row + 1)?;

            // Get the row data.
            let row = self.get_vrow(grid, vrow);

            if let Some(row_cells) = row {
                for (col, cell) in row_cells.iter().enumerate().take(cols) {
                    let is_selected = self.is_in_selection(vrow, col);

                    // Only emit SGR if something changed.
                    if is_selected != cur_selected
                        || cell.fg != cur_fg.unwrap_or(Color::Default)
                        || cell.bg != cur_bg.unwrap_or(Color::Default)
                        || cell.attrs != cur_attrs.unwrap_or_default()
                    {
                        emit_sgr_batch(
                            stdout,
                            cell.fg,
                            cell.bg,
                            cell.attrs,
                            is_selected,
                            &mut cur_fg,
                            &mut cur_bg,
                            &mut cur_attrs,
                            &mut cur_selected,
                        )?;
                    }

                    let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
                    write!(stdout, "{}", ch)?;
                }
                // Fill remaining columns with blank.
                if cols > row_cells.len() {
                    if !cur_selected && cur_fg.is_none() && cur_bg.is_none() {
                        // Already default — just write spaces.
                    } else {
                        emit_reset(stdout)?;
                        cur_fg = None;
                        cur_bg = None;
                        cur_attrs = None;
                        cur_selected = false;
                    }
                    let fill = " ".repeat(cols - row_cells.len());
                    stdout.write_all(fill.as_bytes())?;
                }
            } else {
                // Blank row.
                if cur_selected || cur_fg.is_some() || cur_bg.is_some() {
                    emit_reset(stdout)?;
                    cur_fg = None;
                    cur_bg = None;
                    cur_attrs = None;
                    cur_selected = false;
                }
                let fill = " ".repeat(cols);
                stdout.write_all(fill.as_bytes())?;
            }
        }

        // Reset SGR.
        emit_reset(stdout)?;

        // Position the copy mode cursor.
        let cursor_screen_row = self.vrow.saturating_sub(self.viewport_top) + 1;
        let cursor_screen_col = self.vcol + 1;
        write!(
            stdout,
            "\x1b[{};{}H\x1b[?25h",
            cursor_screen_row, cursor_screen_col
        )?;

        // Render status bar with [copy] indicator.
        let row = view_rows + 1;
        write!(stdout, "\x1b[{};1H\x1b[2K", row)?;
        let copy_status = format!("\x1b[44;97m[copy] \x1b[1;44;93m{}\x1b[0m", status_text);
        let display: String = copy_status.chars().take(term_cols).collect();
        stdout.write_all(display.as_bytes())?;

        stdout.flush()?;
        Ok(())
    }

    /// Check if a (vrow, col) position is within the current selection.
    fn is_in_selection(&self, vrow: usize, col: usize) -> bool {
        let Some(start) = self.selection_start else {
            return false;
        };
        let end = (self.vrow, self.vcol);

        // Normalize.
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };

        let (sr, sc) = start;
        let (er, ec) = end;

        if vrow < sr || vrow > er {
            return false;
        }
        if vrow == sr && vrow == er {
            return col >= sc && col <= ec;
        }
        if vrow == sr {
            return col >= sc;
        }
        if vrow == er {
            return col <= ec;
        }
        true // Middle row — fully selected.
    }
}

/// Emit SGR for a cell, with optional selection highlight.
/// Tracks and updates the current SGR state to minimize output.
#[allow(clippy::too_many_arguments)]
fn emit_sgr_batch(
    stdout: &mut io::Stdout,
    fg: Color,
    bg: Color,
    attrs: crate::grid::Attr,
    selected: bool,
    cur_fg: &mut Option<Color>,
    cur_bg: &mut Option<Color>,
    cur_attrs: &mut Option<crate::grid::Attr>,
    cur_selected: &mut bool,
) -> io::Result<()> {
    stdout.write_all(b"\x1b[0")?;

    if selected {
        // Selection: reverse video.
        stdout.write_all(b";7")?;
        // Swap fg/bg for better colors.
        match fg {
            Color::Default => {}
            Color::Indexed(n) => {
                if n < 8 {
                    write!(stdout, ";{}", 40 + n)?;
                } else if n < 16 {
                    write!(stdout, ";{}", 100 + (n - 8))?;
                } else {
                    write!(stdout, ";48;5;{}", n)?;
                }
            }
            Color::Rgb(r, g, b) => {
                write!(stdout, ";48;2;{};{};{}", r, g, b)?;
            }
        }
        match bg {
            Color::Default => {}
            Color::Indexed(n) => {
                if n < 8 {
                    write!(stdout, ";{}", 30 + n)?;
                } else if n < 16 {
                    write!(stdout, ";{}", 90 + (n - 8))?;
                } else {
                    write!(stdout, ";38;5;{}", n)?;
                }
            }
            Color::Rgb(r, g, b) => {
                write!(stdout, ";38;2;{};{};{}", r, g, b)?;
            }
        }
    } else {
        // Normal cell.
        if attrs.bold {
            stdout.write_all(b";1")?;
        }
        if attrs.italic {
            stdout.write_all(b";3")?;
        }
        if attrs.underline {
            stdout.write_all(b";4")?;
        }
        match fg {
            Color::Default => {}
            Color::Indexed(n) => {
                if n < 8 {
                    write!(stdout, ";{}", 30 + n)?;
                } else if n < 16 {
                    write!(stdout, ";{}", 90 + (n - 8))?;
                } else {
                    write!(stdout, ";38;5;{}", n)?;
                }
            }
            Color::Rgb(r, g, b) => {
                write!(stdout, ";38;2;{};{};{}", r, g, b)?;
            }
        }
        match bg {
            Color::Default => {}
            Color::Indexed(n) => {
                if n < 8 {
                    write!(stdout, ";{}", 40 + n)?;
                } else if n < 16 {
                    write!(stdout, ";{}", 100 + (n - 8))?;
                } else {
                    write!(stdout, ";48;5;{}", n)?;
                }
            }
            Color::Rgb(r, g, b) => {
                write!(stdout, ";48;2;{};{};{}", r, g, b)?;
            }
        }
    }

    stdout.write_all(b"m")?;
    *cur_fg = Some(fg);
    *cur_bg = Some(bg);
    *cur_attrs = Some(attrs);
    *cur_selected = selected;
    Ok(())
}

/// Reset SGR to default.
fn emit_reset(stdout: &mut io::Stdout) -> io::Result<()> {
    stdout.write_all(b"\x1b[0m")
}

/// Copy text to the system clipboard.
/// Auto-detects the clipboard tool: pbcopy (macOS), xclip/xsel (X11), wl-copy (Wayland).
pub fn copy_to_clipboard(text: &str) -> bool {
    // Try pbcopy (macOS).
    if try_clipboard("pbcopy", &[], text) {
        return true;
    }
    // Try wl-copy (Wayland).
    if try_clipboard("wl-copy", &[], text) {
        return true;
    }
    // Try xclip (X11).
    if try_clipboard("xclip", &["-selection", "clipboard"], text) {
        return true;
    }
    // Try xsel (X11).
    if try_clipboard("xsel", &["--clipboard", "--input"], text) {
        return true;
    }
    false
}

/// Try to copy text using a specific clipboard command.
fn try_clipboard(cmd: &str, args: &[&str], text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if let Ok(mut child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(stdin) = child.stdin.as_mut()
            && stdin.write_all(text.as_bytes()).is_ok()
        {
            let _ = child.wait();
            return true;
        }
        let _ = child.kill();
    }
    false
}
