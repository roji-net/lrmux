// Client render: diff-based grid → escape-sequence output.
//
// Compares the current grid to the last-rendered state and emits only
// the escape sequences needed to update changed cells. Consecutive
// changed cells on the same row are batched into a single write to
// avoid per-character cursor positioning.

use std::io::{self, Write};

use crate::grid::{Attr, Cell, Color, Grid};
use crate::term::escapes;

/// The renderer tracks the last-rendered grid state to compute diffs.
pub struct Renderer {
    /// Last-rendered cells (rows × cols). Used for diff comparison.
    prev: Vec<Vec<Cell>>,
    /// Last-rendered cursor position.
    prev_cursor: (usize, usize),
    /// Last-rendered cursor visibility.
    prev_cursor_visible: bool,
    rows: usize,
    cols: usize,
}

impl Renderer {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            prev: vec![vec![Cell::blank(); cols]; rows],
            prev_cursor: (0, 0),
            prev_cursor_visible: true,
            rows,
            cols,
        }
    }

    /// Resize the renderer's internal state.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.prev.resize(rows, vec![Cell::blank(); cols]);
        for row in &mut self.prev {
            row.resize(cols, Cell::blank());
        }
        self.rows = rows;
        self.cols = cols;
        self.prev_cursor = (usize::MAX, usize::MAX);
    }

    /// Render the grid to the given writer. Emits only changed cells.
    pub fn render<W: Write>(&mut self, writer: &mut W, grid: &mut Grid) -> io::Result<()> {
        let mut buf = String::new();
        let mut current_sgr: Option<(Color, Color, Attr)> = None;

        // Only scan rows that were modified since the last render.
        let dirty_rows = grid.take_dirty();

        for row in dirty_rows {
            if row >= self.rows || row >= grid.rows() || row >= self.prev.len() {
                continue;
            }
            let grid_row = match grid.row(row) {
                Some(r) => r,
                None => continue,
            };
            let prev_row = &mut self.prev[row];
            let width = self
                .cols
                .min(grid.cols())
                .min(prev_row.len())
                .min(grid_row.len());
            if width == 0 {
                // Ensure prev_row is sized correctly for future renders.
                prev_row.resize(self.cols.max(1), Cell::blank());
                continue;
            }

            let mut col = 0;
            while col < width {
                // Skip unchanged cells.
                if grid_row[col] == prev_row[col] {
                    col += 1;
                    continue;
                }

                // Found a changed cell. Position cursor here.
                buf.push_str(&escapes::move_cursor((row + 1) as u16, (col + 1) as u16));

                // Emit consecutive changed cells in a batch.
                while col < width && grid_row[col] != prev_row[col] {
                    let cell = &grid_row[col];

                    // Emit SGR if changed.
                    let sgr = (cell.fg, cell.bg, cell.attrs);
                    if current_sgr != Some(sgr) {
                        emit_sgr(&mut buf, cell.fg, cell.bg, cell.attrs);
                        current_sgr = Some(sgr);
                    }

                    // Write the character.
                    if cell.ch == '\0' {
                        buf.push(' ');
                    } else {
                        buf.push(cell.ch);
                    }
                    col += 1;
                }
            }

            // Update prev_row for this row.
            prev_row[..width].clone_from_slice(&grid_row[..width]);
        }

        // If we wrote anything, hide the cursor before writing and show it
        // after, to prevent the user from seeing the cursor jump around
        // as cells are written sequentially.
        if !buf.is_empty() {
            buf.insert_str(0, "\x1b[?25l");
        }

        // Handle cursor.
        let cursor = (grid.cursor_row, grid.cursor_col);
        if grid.cursor_visible {
            if cursor != self.prev_cursor || !self.prev_cursor_visible {
                buf.push_str(&escapes::move_cursor(
                    (cursor.0 + 1) as u16,
                    (cursor.1 + 1) as u16,
                ));
            }
            // Always show cursor at the end if it should be visible.
            buf.push_str("\x1b[?25h");
        } else if self.prev_cursor_visible {
            buf.push_str("\x1b[?25l");
        }

        self.prev_cursor = cursor;
        self.prev_cursor_visible = grid.cursor_visible;

        if !buf.is_empty() {
            writer.write_all(buf.as_bytes())?;
            writer.flush()?;
        }

        Ok(())
    }

    /// Force a full redraw on next render by invalidating all prev cells.
    pub fn invalidate(&mut self) {
        for row in &mut self.prev {
            for cell in row.iter_mut() {
                *cell = Cell::blank();
            }
        }
        self.prev_cursor = (usize::MAX, usize::MAX);
    }
}

/// Emit SGR escape sequence for the given colors and attributes.
fn emit_sgr(buf: &mut String, fg: Color, bg: Color, attrs: Attr) {
    buf.push_str("\x1b[0"); // Reset first

    if attrs.bold {
        buf.push_str(";1");
    }
    if attrs.italic {
        buf.push_str(";3");
    }
    if attrs.underline {
        buf.push_str(";4");
    }
    if attrs.reverse {
        buf.push_str(";7");
    }

    match fg {
        Color::Default => {}
        Color::Indexed(n) => {
            if n < 8 {
                buf.push_str(&format!(";{}", 30 + n));
            } else if n < 16 {
                buf.push_str(&format!(";{}", 90 + (n - 8)));
            } else {
                buf.push_str(&format!(";38;5;{}", n));
            }
        }
        Color::Rgb(r, g, b) => {
            buf.push_str(&format!(";38;2;{};{};{}", r, g, b));
        }
    }

    match bg {
        Color::Default => {}
        Color::Indexed(n) => {
            if n < 8 {
                buf.push_str(&format!(";{}", 40 + n));
            } else if n < 16 {
                buf.push_str(&format!(";{}", 100 + (n - 8)));
            } else {
                buf.push_str(&format!(";48;5;{}", n));
            }
        }
        Color::Rgb(r, g, b) => {
            buf.push_str(&format!(";48;2;{};{};{}", r, g, b));
        }
    }

    buf.push('m');
}
