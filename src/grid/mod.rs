// Grid: screen grid + scrollback ring buffer, cursor, terminal state.

pub mod cell;
pub mod scrollback;

pub use cell::{Attr, Cell, Color};
use scrollback::Scrollback;

/// The terminal screen grid.
///
/// Stores the visible rows plus scrollback history. The cursor position
/// and current text attributes (fg/bg/attrs) are tracked here so the VT
/// parser can update them as it processes escape sequences.
pub struct Grid {
    rows: Vec<Vec<Cell>>,
    cols: usize,
    row_count: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    /// Current foreground color for newly printed chars.
    pub fg: Color,
    /// Current background color for newly printed chars.
    pub bg: Color,
    /// Current text attributes for newly printed chars.
    pub attrs: Attr,
    /// Scrollback history (rows that scrolled off the top).
    pub scrollback: Scrollback,
    /// New scrollback rows not yet sent to clients. Drained by take_pending_scrollback().
    pending_scrollback: Vec<Vec<Cell>>,
    /// Scroll region top (inclusive), 0-indexed.
    pub scroll_top: usize,
    /// Scroll region bottom (exclusive), 0-indexed.
    pub scroll_bottom: usize,
    /// Whether the next print should wrap to the next line.
    wrap_pending: bool,
    /// Saved cursor position (for DECSC/DECRC).
    saved_cursor: Option<(usize, usize)>,
    /// Application cursor keys mode (DECCKM). When true, arrow keys
    /// should be translated from \x1b[A/B/C/D to \x1bOA/B/C/D.
    pub app_cursor_keys: bool,
    /// Rows modified since the last render. The renderer uses this to
    /// skip unchanged rows instead of scanning the entire grid.
    dirty: Vec<bool>,
}

impl Grid {
    pub fn new(rows: usize, cols: usize, scrollback_capacity: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let mut grid = Self {
            rows: (0..rows).map(|_| vec![Cell::blank(); cols]).collect(),
            cols,
            row_count: rows,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attr::default(),
            scrollback: Scrollback::new(scrollback_capacity),
            pending_scrollback: Vec::new(),
            scroll_top: 0,
            scroll_bottom: rows,
            wrap_pending: false,
            saved_cursor: None,
            app_cursor_keys: false,
            dirty: vec![true; rows],
        };
        grid.scroll_bottom = rows;
        grid
    }

    pub fn rows(&self) -> usize {
        self.row_count
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Mark a row as dirty (modified since last render).
    #[inline]
    pub fn mark_dirty(&mut self, row: usize) {
        if row < self.dirty.len() {
            self.dirty[row] = true;
        }
    }

    /// Mark all rows as dirty.
    pub fn mark_all_dirty(&mut self) {
        self.dirty.fill(true);
    }

    /// Take the dirty row set, returning a vector of dirty row indices
    /// and clearing the dirty flags. Called by the renderer after scanning.
    pub fn take_dirty(&mut self) -> Vec<usize> {
        let mut dirty_rows = Vec::new();
        for (i, d) in self.dirty.iter_mut().enumerate() {
            if *d {
                dirty_rows.push(i);
                *d = false;
            }
        }
        dirty_rows
    }

    /// Take pending scrollback rows (rows that scrolled off the top since
    /// the last call). The server sends these to clients so they can
    /// maintain their own scrollback for copy mode.
    pub fn take_pending_scrollback(&mut self) -> Vec<Vec<Cell>> {
        std::mem::take(&mut self.pending_scrollback)
    }

    /// Resize the grid. Content is preserved where possible; new cells are blank.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        // Adjust row count.
        if rows > self.row_count {
            // Add blank rows at the bottom.
            for _ in self.row_count..rows {
                self.rows.push(vec![Cell::blank(); cols]);
            }
        } else if rows < self.row_count {
            // Remove rows from the bottom, pushing them to scrollback.
            for _ in rows..self.row_count {
                let row = self.rows.remove(self.rows.len() - 1);
                // Only push non-blank rows to scrollback.
                self.scrollback.push(row.clone());
                self.pending_scrollback.push(row);
            }
        }

        // Adjust column count for each row.
        for row in &mut self.rows {
            if cols > row.len() {
                row.resize(cols, Cell::blank());
            } else if cols < row.len() {
                row.truncate(cols);
            }
        }

        self.row_count = rows;
        self.cols = cols;
        self.scroll_top = 0;
        self.scroll_bottom = rows;
        self.dirty.resize(rows, true);
        self.mark_all_dirty();
        // Clamp cursor.
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        self.wrap_pending = false;
    }

    /// Get a reference to a row. Returns None if out of bounds.
    pub fn row(&self, row: usize) -> Option<&[Cell]> {
        self.rows.get(row).map(|r| &r[..])
    }

    /// Get a mutable reference to a row. Returns None if out of bounds.
    pub fn row_mut(&mut self, row: usize) -> Option<&mut Vec<Cell>> {
        self.rows.get_mut(row)
    }

    /// Get a specific cell. Returns None if out of bounds.
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        self.rows.get(row).and_then(|r| r.get(col))
    }

    /// Print a character at the cursor position, advancing the cursor.
    pub fn print(&mut self, c: char) {
        // Handle wrap: if we were at the last column and need to wrap.
        if self.wrap_pending {
            self.cursor_col = 0;
            self.cursor_row += 1;
            self.wrap_pending = false;
            if self.cursor_row >= self.scroll_bottom {
                self.scroll_up(1);
                self.cursor_row = self.scroll_bottom.saturating_sub(1);
            }
        }

        if self.cursor_col >= self.cols {
            // Should not happen if wrap_pending is handled, but clamp just in case.
            self.cursor_col = self.cols.saturating_sub(1);
        }

        // Write the cell.
        let (fg, bg, attrs) = (self.fg, self.bg, self.attrs);
        let (crow, ccol) = (self.cursor_row, self.cursor_col);
        if let Some(row) = self.rows.get_mut(crow)
            && ccol < row.len()
        {
            row[ccol] = Cell {
                ch: c,
                fg,
                bg,
                attrs,
            };
        }
        self.mark_dirty(crow);

        // Advance cursor.
        self.cursor_col += 1;
        if self.cursor_col >= self.cols {
            // Wrap to next line on next print.
            self.cursor_col = self.cols.saturating_sub(1);
            self.wrap_pending = true;
        }
    }

    /// Move cursor to absolute position.
    pub fn move_cursor(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.row_count.saturating_sub(1));
        self.cursor_col = col.min(self.cols.saturating_sub(1));
        self.wrap_pending = false;
    }

    /// Move cursor relative to current position.
    pub fn move_cursor_rel(&mut self, drow: i32, dcol: i32) {
        let new_row = self.cursor_row as i32 + drow;
        let new_col = self.cursor_col as i32 + dcol;
        self.move_cursor(new_row.max(0) as usize, new_col.max(0) as usize);
    }

    /// Carriage return: move cursor to column 0.
    pub fn carriage_return(&mut self) {
        self.cursor_col = 0;
        self.wrap_pending = false;
    }

    /// Line feed: move cursor down one row, scrolling if needed.
    pub fn line_feed(&mut self) {
        self.cursor_row += 1;
        if self.cursor_row >= self.scroll_bottom {
            self.scroll_up(1);
            self.cursor_row = self.scroll_bottom - 1;
        }
        self.wrap_pending = false;
    }

    /// Backspace: move cursor left one column.
    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
        self.wrap_pending = false;
    }

    /// Tab: move cursor to next multiple of 8.
    pub fn tab(&mut self) {
        let next = (self.cursor_col / 8 + 1) * 8;
        self.cursor_col = next.min(self.cols.saturating_sub(1));
        self.wrap_pending = false;
    }

    /// Scroll the scroll region up by n lines. Lines that scroll off the top
    /// go to the scrollback buffer.
    pub fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.scroll_bottom - self.scroll_top);
        if n == 0 {
            return;
        }

        // Save scrolled-off rows to scrollback, replacing with blank rows
        // of the correct width so the Vec never becomes empty.
        let blank_row = vec![Cell::blank(); self.cols];
        for i in 0..n {
            let row_idx = self.scroll_top + i;
            if row_idx < self.rows.len() {
                let row = std::mem::replace(&mut self.rows[row_idx], blank_row.clone());
                self.scrollback.push(row.clone());
                self.pending_scrollback.push(row);
            }
        }

        // Shift rows up: rotate the scroll region left by n.
        // The blank rows we inserted at the top end up at the bottom.
        self.rows[self.scroll_top..self.scroll_bottom].rotate_left(n);

        // All rows in the scroll region changed.
        for i in self.scroll_top..self.scroll_bottom {
            self.mark_dirty(i);
        }
    }

    /// Scroll the scroll region down by n lines (e.g., for reverse line feed).
    /// Content moves down; the top n rows become blank; rows scrolled off the
    /// bottom are discarded (not added to scrollback).
    pub fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.scroll_bottom - self.scroll_top);
        if n == 0 {
            return;
        }

        // Rotate first, then blank the top. Blanking *before* rotate_right
        // incorrectly leaves the former bottom row at the top — which is
        // exactly the ghost-text vim Ctrl-B / reverse-scroll bug: vim then
        // writes a short line over that stale row without EL, leaving the
        // leftover tail (`====…`, mashed comments, etc.).
        self.rows[self.scroll_top..self.scroll_bottom].rotate_right(n);
        let blank_row = vec![Cell::blank(); self.cols];
        for i in 0..n {
            self.rows[self.scroll_top + i] = blank_row.clone();
        }

        for i in self.scroll_top..self.scroll_bottom {
            self.mark_dirty(i);
        }
    }

    /// Insert n blank lines at the cursor row, scrolling lines within the
    /// scroll region down. Lines that scroll off the bottom are lost.
    /// (IL — Insert Line, CSI L)
    pub fn insert_lines(&mut self, n: usize) {
        let cursor = self.cursor_row;
        if cursor < self.scroll_top || cursor >= self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - cursor);
        if n == 0 {
            return;
        }

        // Rotate the region [cursor..scroll_bottom] right by n.
        // This brings the bottom n rows to the top of the region.
        self.rows[cursor..self.scroll_bottom].rotate_right(n);
        // Blank the first n rows of the region (the newly inserted blanks).
        let blank_row = vec![Cell::blank(); self.cols];
        for i in 0..n {
            self.rows[cursor + i] = blank_row.clone();
        }

        for i in cursor..self.scroll_bottom {
            self.mark_dirty(i);
        }
    }

    /// Delete n lines at the cursor row, scrolling lines within the scroll
    /// region up. Blank lines appear at the bottom of the scroll region.
    /// (DL — Delete Line, CSI M)
    pub fn delete_lines(&mut self, n: usize) {
        let cursor = self.cursor_row;
        if cursor < self.scroll_top || cursor >= self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - cursor);
        if n == 0 {
            return;
        }

        // Rotate the region [cursor..scroll_bottom] left by n.
        // This brings rows after cursor up by n positions.
        self.rows[cursor..self.scroll_bottom].rotate_left(n);
        // Blank the last n rows of the region (the newly freed space).
        let blank_row = vec![Cell::blank(); self.cols];
        for i in 0..n {
            self.rows[self.scroll_bottom - 1 - i] = blank_row.clone();
        }

        for i in cursor..self.scroll_bottom {
            self.mark_dirty(i);
        }
    }

    /// Insert n blank characters at the cursor, shifting the rest of the
    /// line right. Characters shifted off the right edge are discarded.
    /// (ICH — Insert Character, CSI @)
    pub fn insert_chars(&mut self, n: usize) {
        let (crow, ccol) = (self.cursor_row, self.cursor_col);
        let Some(row) = self.rows.get_mut(crow) else {
            return;
        };
        if ccol >= row.len() {
            return;
        }
        let n = n.min(row.len() - ccol);
        if n == 0 {
            return;
        }
        row[ccol..].rotate_right(n);
        for cell in &mut row[ccol..ccol + n] {
            *cell = Cell::blank();
        }
        self.wrap_pending = false;
        self.mark_dirty(crow);
    }

    /// Delete n characters at the cursor, shifting the rest of the line
    /// left. Blank cells fill the end of the line.
    /// (DCH — Delete Character, CSI P)
    pub fn delete_chars(&mut self, n: usize) {
        let (crow, ccol) = (self.cursor_row, self.cursor_col);
        let Some(row) = self.rows.get_mut(crow) else {
            return;
        };
        if ccol >= row.len() {
            return;
        }
        let n = n.min(row.len() - ccol);
        if n == 0 {
            return;
        }
        row[ccol..].rotate_left(n);
        let len = row.len();
        for cell in &mut row[len - n..] {
            *cell = Cell::blank();
        }
        self.wrap_pending = false;
        self.mark_dirty(crow);
    }

    /// Erase n characters starting at the cursor (replace with blanks).
    /// Does not shift the rest of the line.
    /// (ECH — Erase Character, CSI X)
    pub fn erase_chars(&mut self, n: usize) {
        let (crow, ccol) = (self.cursor_row, self.cursor_col);
        let Some(row) = self.rows.get_mut(crow) else {
            return;
        };
        if ccol >= row.len() {
            return;
        }
        let end = (ccol + n).min(row.len());
        row[ccol..end].fill(Cell::blank());
        self.wrap_pending = false;
        self.mark_dirty(crow);
    }

    /// Erase from cursor to end of line.
    pub fn erase_to_end_of_line(&mut self) {
        let (crow, ccol) = (self.cursor_row, self.cursor_col);
        if let Some(row) = self.rows.get_mut(crow) {
            let start = ccol.min(row.len());
            row[start..].fill(Cell::blank());
        }
        self.mark_dirty(crow);
    }

    /// Erase from start of line to cursor (inclusive).
    pub fn erase_to_cursor(&mut self) {
        let (crow, ccol) = (self.cursor_row, self.cursor_col);
        if let Some(row) = self.rows.get_mut(crow) {
            if row.is_empty() {
                return;
            }
            let end = ccol.min(row.len() - 1);
            row[..=end].fill(Cell::blank());
        }
        self.mark_dirty(crow);
    }

    /// Erase the entire current line.
    pub fn erase_line(&mut self) {
        let crow = self.cursor_row;
        if let Some(row) = self.row_mut(crow) {
            row.fill(Cell::blank());
        }
        self.mark_dirty(crow);
    }

    /// Erase from cursor to end of screen.
    pub fn erase_to_end_of_screen(&mut self) {
        self.erase_to_end_of_line();
        for i in (self.cursor_row + 1)..self.row_count {
            if let Some(row) = self.row_mut(i) {
                row.fill(Cell::blank());
            }
            self.mark_dirty(i);
        }
    }

    /// Erase from start of screen to cursor (inclusive).
    pub fn erase_to_start_of_screen(&mut self) {
        self.erase_to_cursor();
        for i in 0..self.cursor_row {
            if let Some(row) = self.row_mut(i) {
                row.fill(Cell::blank());
            }
            self.mark_dirty(i);
        }
    }

    /// Erase the entire screen.
    pub fn erase_screen(&mut self) {
        for i in 0..self.row_count {
            if let Some(row) = self.row_mut(i) {
                row.fill(Cell::blank());
            }
            self.mark_dirty(i);
        }
    }

    /// Set the scroll region (DECSTBM). Both are 0-indexed, top inclusive, bottom exclusive.
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        self.scroll_top = top.min(self.row_count);
        self.scroll_bottom = bottom.min(self.row_count).max(top + 1);
    }

    /// Reset the scroll region to the full screen.
    pub fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.row_count;
    }

    /// Set the current text attributes from SGR parameters.
    pub fn set_sgr(&mut self, params: &[u16]) {
        if params.is_empty() {
            self.fg = Color::Default;
            self.bg = Color::Default;
            self.attrs = Attr::default();
            return;
        }

        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => {
                    self.fg = Color::Default;
                    self.bg = Color::Default;
                    self.attrs = Attr::default();
                }
                1 => self.attrs.bold = true,
                3 => self.attrs.italic = true,
                4 => self.attrs.underline = true,
                7 => self.attrs.reverse = true,
                22 => self.attrs.bold = false,
                23 => self.attrs.italic = false,
                24 => self.attrs.underline = false,
                27 => self.attrs.reverse = false,
                38 => {
                    // Foreground color
                    if i + 1 < params.len() {
                        match params[i + 1] {
                            2 => {
                                // Truecolor: 38;2;R;G;B
                                if i + 4 < params.len() {
                                    self.fg = Color::Rgb(
                                        params[i + 2] as u8,
                                        params[i + 3] as u8,
                                        params[i + 4] as u8,
                                    );
                                    i += 4;
                                }
                            }
                            5 if i + 2 < params.len() => {
                                // 256-color: 38;5;N
                                self.fg = Color::Indexed(params[i + 2] as u8);
                                i += 2;
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                }
                39 => self.fg = Color::Default,
                48 => {
                    // Background color
                    if i + 1 < params.len() {
                        match params[i + 1] {
                            2 => {
                                // Truecolor: 48;2;R;G;B
                                if i + 4 < params.len() {
                                    self.bg = Color::Rgb(
                                        params[i + 2] as u8,
                                        params[i + 3] as u8,
                                        params[i + 4] as u8,
                                    );
                                    i += 4;
                                }
                            }
                            5 if i + 2 < params.len() => {
                                // 256-color: 48;5;N
                                self.bg = Color::Indexed(params[i + 2] as u8);
                                i += 2;
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                }
                49 => self.bg = Color::Default,
                30..=37 => {
                    // Standard 16-color foreground
                    self.fg = Color::Indexed(params[i] as u8 - 30);
                }
                90..=97 => {
                    // Bright foreground
                    self.fg = Color::Indexed(params[i] as u8 - 90 + 8);
                }
                40..=47 => {
                    // Standard 16-color background
                    self.bg = Color::Indexed(params[i] as u8 - 40);
                }
                100..=107 => {
                    // Bright background
                    self.bg = Color::Indexed(params[i] as u8 - 100 + 8);
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Save the current cursor position.
    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col));
    }

    /// Restore the saved cursor position.
    pub fn restore_cursor(&mut self) {
        if let Some((row, col)) = self.saved_cursor {
            self.move_cursor(row, col);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_chars(grid: &Grid, row: usize) -> String {
        grid.row(row)
            .unwrap()
            .iter()
            .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn fill_row(grid: &mut Grid, row: usize, ch: char) {
        if let Some(r) = grid.row_mut(row) {
            for cell in r.iter_mut() {
                cell.ch = ch;
            }
        }
    }

    #[test]
    fn scroll_down_blanks_top_not_bottom() {
        // 5-row grid filled with distinct markers.
        let mut grid = Grid::new(5, 8, 100);
        for (i, ch) in ['A', 'B', 'C', 'D', 'E'].iter().enumerate() {
            fill_row(&mut grid, i, *ch);
        }

        grid.scroll_down(1);

        assert_eq!(row_chars(&grid, 0), "", "top row must be blank");
        assert_eq!(row_chars(&grid, 1), "AAAAAAAA");
        assert_eq!(row_chars(&grid, 2), "BBBBBBBB");
        assert_eq!(row_chars(&grid, 3), "CCCCCCCC");
        assert_eq!(row_chars(&grid, 4), "DDDDDDDD");
        // E must be discarded — the old bug left it on row 0.
    }

    #[test]
    fn scroll_down_within_region() {
        let mut grid = Grid::new(5, 4, 100);
        for (i, ch) in ['A', 'B', 'C', 'D', 'E'].iter().enumerate() {
            fill_row(&mut grid, i, *ch);
        }
        // Exclude top and bottom rows (vim-style status/chrome).
        grid.set_scroll_region(1, 4);

        grid.scroll_down(1);

        assert_eq!(row_chars(&grid, 0), "AAAA"); // outside region unchanged
        assert_eq!(row_chars(&grid, 1), ""); // blank inserted at region top
        assert_eq!(row_chars(&grid, 2), "BBBB");
        assert_eq!(row_chars(&grid, 3), "CCCC");
        assert_eq!(row_chars(&grid, 4), "EEEE"); // outside region unchanged
    }

    #[test]
    fn scroll_up_blanks_bottom() {
        let mut grid = Grid::new(5, 4, 100);
        for (i, ch) in ['A', 'B', 'C', 'D', 'E'].iter().enumerate() {
            fill_row(&mut grid, i, *ch);
        }

        grid.scroll_up(1);

        assert_eq!(row_chars(&grid, 0), "BBBB");
        assert_eq!(row_chars(&grid, 4), "");
    }

    #[test]
    fn delete_chars_shifts_left() {
        let mut grid = Grid::new(1, 8, 100);
        for ch in "hello".chars() {
            grid.print(ch);
        }
        grid.move_cursor(0, 0);
        grid.delete_chars(1);
        assert_eq!(row_chars(&grid, 0), "ello");
        assert_eq!(grid.cursor_col, 0);
    }

    #[test]
    fn insert_chars_shifts_right() {
        let mut grid = Grid::new(1, 8, 100);
        for ch in "ello".chars() {
            grid.print(ch);
        }
        grid.move_cursor(0, 0);
        grid.insert_chars(1);
        // blank + ello
        assert_eq!(row_chars(&grid, 0), " ello");
        grid.print('h');
        assert_eq!(row_chars(&grid, 0), "hello");
    }

    #[test]
    fn erase_chars_no_shift() {
        let mut grid = Grid::new(1, 8, 100);
        for ch in "hello".chars() {
            grid.print(ch);
        }
        grid.move_cursor(0, 1);
        grid.erase_chars(2);
        assert_eq!(row_chars(&grid, 0), "h  lo");
    }
}
