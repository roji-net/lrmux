// Client render: diff-based grid → escape-sequence output.
//
// Compares the current grid to the last-rendered state and emits only
// the escape sequences needed to update changed cells. Consecutive
// changed cells on the same row are batched into a single write to
// avoid per-character cursor positioning.
//
// Viewport support: the renderer only writes cells within the viewport
// (the visible portion of the canonical grid). If the terminal is
// smaller than the grid, the grid is cropped. If larger, the client
// fills the remaining area with a filler region.

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
    /// Canonical grid dimensions (the full grid from the server).
    rows: usize,
    cols: usize,
    /// Viewport dimensions (the visible portion of the grid).
    /// view_rows = min(grid_rows, term_rows - 1)  (minus 1 for status bar)
    /// view_cols = min(grid_cols, term_cols)
    view_rows: usize,
    view_cols: usize,
}

impl Renderer {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            prev: vec![vec![Cell::blank(); cols]; rows],
            prev_cursor: (0, 0),
            prev_cursor_visible: true,
            rows,
            cols,
            view_rows: rows,
            view_cols: cols,
        }
    }

    /// Resize the renderer's internal state (canonical grid dimensions).
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.prev.resize(rows, vec![Cell::blank(); cols]);
        for row in &mut self.prev {
            row.resize(cols, Cell::blank());
        }
        self.rows = rows;
        self.cols = cols;
        self.prev_cursor = (usize::MAX, usize::MAX);
        // Clamp viewport to new grid size.
        self.view_rows = self.view_rows.min(rows);
        self.view_cols = self.view_cols.min(cols);
    }

    /// Set the viewport size (the visible portion of the grid).
    /// Called when the terminal is resized or the canonical grid changes.
    pub fn set_viewport(&mut self, view_rows: usize, view_cols: usize) {
        self.view_rows = view_rows.min(self.rows);
        self.view_cols = view_cols.min(self.cols);
        // Invalidate so the next render redraws everything within the new viewport.
        self.invalidate();
    }

    /// Get the current viewport dimensions.
    pub fn viewport(&self) -> (usize, usize) {
        (self.view_rows, self.view_cols)
    }

    /// Render the grid to the given writer. Emits only changed cells within the viewport.
    pub fn render<W: Write>(&mut self, writer: &mut W, grid: &mut Grid) -> io::Result<()> {
        let mut buf = String::new();
        let mut current_sgr: Option<(Color, Color, Attr)> = None;

        // Only scan rows that were modified since the last render.
        let dirty_rows = grid.take_dirty();

        for row in dirty_rows {
            // Skip rows outside the viewport.
            if row >= self.view_rows || row >= grid.rows() || row >= self.prev.len() {
                continue;
            }
            let grid_row = match grid.row(row) {
                Some(r) => r,
                None => continue,
            };
            let prev_row = &mut self.prev[row];
            let width = self
                .view_cols
                .min(grid.cols())
                .min(prev_row.len())
                .min(grid_row.len());
            if width == 0 {
                continue;
            }

            let mut col = 0;
            while col < width {
                // Skip unchanged cells.
                if grid_row[col] == prev_row[col] {
                    col += 1;
                    continue;
                }

                // Found a changed cell. Position cursor here (1-based terminal coords).
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

            // Update prev_row for this row (only the viewport portion).
            prev_row[..width].clone_from_slice(&grid_row[..width]);
        }

        // If we wrote anything, hide the cursor before writing and show it
        // after, to prevent the user from seeing the cursor jump around
        // as cells are written sequentially.
        let cell_wrote = !buf.is_empty();
        if cell_wrote {
            buf.insert_str(0, "\x1b[?25l");
        }

        // Handle cursor — only show if within the viewport.
        let cursor = (grid.cursor_row, grid.cursor_col);
        let cursor_in_viewport =
            grid.cursor_row < self.view_rows && grid.cursor_col < self.view_cols;
        if grid.cursor_visible && cursor_in_viewport {
            if cursor != self.prev_cursor || !self.prev_cursor_visible {
                // Hide cursor before moving it (if not already hidden by cell writes)
                // to avoid the cursor briefly appearing at old positions.
                if !cell_wrote {
                    buf.push_str("\x1b[?25l");
                }
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
        self.prev_cursor_visible = grid.cursor_visible && cursor_in_viewport;

        if !buf.is_empty() {
            writer.write_all(buf.as_bytes())?;
            writer.flush()?;
        }

        Ok(())
    }

    /// Force a full redraw on next render by invalidating all prev cells.
    pub fn invalidate(&mut self) {
        for row in &mut self.prev {
            row.fill(Cell::blank());
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
