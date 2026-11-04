//! Multi-pane window view: per-pane grids + composite for the renderer.

use std::collections::HashMap;

use crate::grid::{Attr, Cell, Color, Grid};
use crate::proto::PaneRect;

/// One pane's local grid and its rectangle inside the window.
pub struct PaneView {
    pub rect: PaneRect,
    pub grid: Grid,
}

/// Client-side model of a window with zero or more panes.
///
/// The [`composite`](Self::composite) grid is what the existing diff
/// renderer paints. Pane grids hold per-pane content and scrollback;
/// [`rebuild_composite`](Self::rebuild_composite) blits them plus borders.
pub struct WindowView {
    pub win_rows: usize,
    pub win_cols: usize,
    pub active_pane: u32,
    pub panes: HashMap<u32, PaneView>,
    pub composite: Grid,
}

impl std::ops::Deref for WindowView {
    type Target = Grid;
    fn deref(&self) -> &Grid {
        &self.composite
    }
}

impl std::ops::DerefMut for WindowView {
    fn deref_mut(&mut self) -> &mut Grid {
        &mut self.composite
    }
}

impl WindowView {
    /// Single-pane window matching a legacy snapshot (no layout msg yet).
    pub fn single(rows: usize, cols: usize) -> Self {
        let mut panes = HashMap::new();
        let id = 0;
        panes.insert(
            id,
            PaneView {
                rect: PaneRect {
                    pane_id: id,
                    x: 0,
                    y: 0,
                    cols: cols as u16,
                    rows: rows as u16,
                },
                grid: Grid::new(rows, cols, 10_000),
            },
        );
        Self {
            win_rows: rows.max(1),
            win_cols: cols.max(1),
            active_pane: id,
            panes,
            composite: Grid::new(rows.max(1), cols.max(1), 10_000),
        }
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub fn active(&self) -> Option<&PaneView> {
        self.panes.get(&self.active_pane)
    }

    pub fn active_mut(&mut self) -> Option<&mut PaneView> {
        self.panes.get_mut(&self.active_pane)
    }

    /// Apply a [`ServerMsg::WindowLayout`](crate::proto::ServerMsg::WindowLayout).
    /// Keeps existing pane grids when ids match; creates blank grids for new ids.
    pub fn apply_layout(&mut self, rows: u16, cols: u16, active: u32, rects: &[PaneRect]) {
        self.win_rows = rows.max(1) as usize;
        self.win_cols = cols.max(1) as usize;
        self.active_pane = active;

        let mut next = HashMap::new();
        for r in rects {
            let pr = r.rows.max(1) as usize;
            let pc = r.cols.max(1) as usize;
            let grid = if let Some(mut old) = self.panes.remove(&r.pane_id) {
                if old.grid.rows() != pr || old.grid.cols() != pc {
                    old.grid.resize(pr, pc);
                }
                old.grid
            } else {
                Grid::new(pr, pc, 10_000)
            };
            next.insert(r.pane_id, PaneView { rect: *r, grid });
        }
        self.panes = next;
        if self.composite.rows() != self.win_rows || self.composite.cols() != self.win_cols {
            self.composite = Grid::new(self.win_rows, self.win_cols, 10_000);
        }
        self.rebuild_composite();
    }

    /// Replace one pane's cells from a full snapshot.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_pane_snapshot(
        &mut self,
        pane_id: Option<u32>,
        rows: u16,
        cols: u16,
        cells: &[Cell],
        cursor_row: u16,
        cursor_col: u16,
        cursor_visible: bool,
        mouse_flags: u8,
    ) {
        let pid = pane_id.unwrap_or(self.active_pane);
        // Legacy single-pane: reset the whole view to one pane.
        if pane_id.is_none() && self.panes.len() <= 1 {
            *self = Self::single(rows as usize, cols as usize);
            self.active_pane = 0;
            if let Some(p) = self.panes.get_mut(&0) {
                fill_grid_from_cells(&mut p.grid, cols as usize, cells);
                p.grid.cursor_row = cursor_row as usize;
                p.grid.cursor_col = cursor_col as usize;
                p.grid.cursor_visible = cursor_visible;
                p.grid.set_mouse_flags(mouse_flags);
            }
            self.rebuild_composite();
            return;
        }

        self.panes.entry(pid).or_insert_with(|| PaneView {
            rect: PaneRect {
                pane_id: pid,
                x: 0,
                y: 0,
                cols,
                rows,
            },
            grid: Grid::new(rows.max(1) as usize, cols.max(1) as usize, 10_000),
        });
        if let Some(p) = self.panes.get_mut(&pid) {
            if p.grid.rows() != rows as usize || p.grid.cols() != cols as usize {
                p.grid = Grid::new(rows.max(1) as usize, cols.max(1) as usize, 10_000);
            }
            fill_grid_from_cells(&mut p.grid, cols as usize, cells);
            p.grid.cursor_row = cursor_row as usize;
            p.grid.cursor_col = cursor_col as usize;
            p.grid.cursor_visible = cursor_visible;
            p.grid.set_mouse_flags(mouse_flags);
            p.rect.rows = rows;
            p.rect.cols = cols;
        }
        self.blit_pane(pid);
        self.paint_borders();
        self.sync_active_cursor();
        self.composite.mark_all_dirty();
    }

    /// Apply dirty rows to a pane and blit into the composite.
    pub fn apply_pane_update(
        &mut self,
        pane_id: Option<u32>,
        dirty: &[(u16, Vec<Cell>)],
        cursor_row: u16,
        cursor_col: u16,
        cursor_visible: bool,
        mouse_flags: u8,
    ) {
        let pid = pane_id.unwrap_or(self.active_pane);
        let Some(p) = self.panes.get_mut(&pid) else {
            return;
        };
        for (row, cells) in dirty {
            apply_row(&mut p.grid, *row as usize, cells);
        }
        p.grid.cursor_row = cursor_row as usize;
        p.grid.cursor_col = cursor_col as usize;
        p.grid.cursor_visible = cursor_visible;
        p.grid.set_mouse_flags(mouse_flags);
        let rect = p.rect;
        let inactive = pid != self.active_pane && self.panes.len() > 1;
        // Blit only dirty rows into composite.
        for (row, _) in dirty {
            let pr = *row as usize;
            if let Some(src) = self.panes.get(&pid).and_then(|p| p.grid.row(pr)) {
                let cy = rect.y as usize + pr;
                if cy < self.composite.rows()
                    && let Some(dst) = self.composite.row_mut(cy)
                {
                    let x0 = rect.x as usize;
                    for (i, cell) in src.iter().enumerate() {
                        if x0 + i < dst.len() {
                            dst[x0 + i] = composite_cell(cell, inactive);
                        }
                    }
                    self.composite.mark_dirty(cy);
                }
            }
        }
        if pid == self.active_pane {
            self.sync_active_cursor();
            if let Some(p) = self.panes.get(&pid) {
                self.composite.set_mouse_flags(p.grid.mouse_flags());
            }
        }
    }

    pub fn push_scrollback(&mut self, pane_id: Option<u32>, rows: Vec<Vec<Cell>>) {
        let pid = pane_id.unwrap_or(self.active_pane);
        if let Some(p) = self.panes.get_mut(&pid) {
            for row in rows {
                p.grid.scrollback.push(row);
            }
        }
    }

    /// Full recompose (layout change / snapshot).
    pub fn rebuild_composite(&mut self) {
        if self.composite.rows() != self.win_rows || self.composite.cols() != self.win_cols {
            self.composite = Grid::new(self.win_rows, self.win_cols, 10_000);
        } else {
            // Clear to blanks before blit.
            for r in 0..self.win_rows {
                if let Some(row) = self.composite.row_mut(r) {
                    for c in row.iter_mut() {
                        *c = Cell::blank();
                    }
                }
            }
        }
        let ids: Vec<u32> = self.panes.keys().copied().collect();
        for id in ids {
            self.blit_pane(id);
        }
        self.paint_borders();
        self.sync_active_cursor();
        self.composite.mark_all_dirty();
    }

    fn blit_pane(&mut self, pane_id: u32) {
        let inactive = pane_id != self.active_pane && self.panes.len() > 1;
        let Some(p) = self.panes.get(&pane_id) else {
            return;
        };
        let rect = p.rect;
        let pr = p.grid.rows();
        let pc = p.grid.cols();
        for r in 0..pr {
            let Some(src) = p.grid.row(r) else { continue };
            let cy = rect.y as usize + r;
            if cy >= self.composite.rows() {
                break;
            }
            let x0 = rect.x as usize;
            if let Some(dst) = self.composite.row_mut(cy) {
                for c in 0..pc {
                    if x0 + c < dst.len() && c < src.len() {
                        dst[x0 + c] = composite_cell(&src[c], inactive);
                    }
                }
                self.composite.mark_dirty(cy);
            }
        }
        let _ = pc;
    }

    /// Draw separator cells in gaps between panes.
    ///
    /// Straight edges adjacent to the active pane use `^v<>` pointing
    /// *toward* the focus (tmux-like "which side is active?"); junctions
    /// keep box-drawing. Active-adjacent borders are bold cyan.
    fn paint_borders(&mut self) {
        if self.panes.len() <= 1 {
            return;
        }
        // Collect occupied pane cells; anything left blank in the composite
        // that sits between panes becomes a border glyph.
        let mut occupied = vec![false; self.win_rows * self.win_cols];
        for p in self.panes.values() {
            for r in 0..p.rect.rows as usize {
                for c in 0..p.rect.cols as usize {
                    let y = p.rect.y as usize + r;
                    let x = p.rect.x as usize + c;
                    if y < self.win_rows && x < self.win_cols {
                        occupied[y * self.win_cols + x] = true;
                    }
                }
            }
        }
        let active = self.active_pane;
        let active_rect = self.panes.get(&active).map(|p| p.rect);
        for y in 0..self.win_rows {
            for x in 0..self.win_cols {
                if occupied[y * self.win_cols + x] {
                    continue;
                }
                let near_active = active_rect
                    .map(|r| is_adjacent_to_rect(x, y, r))
                    .unwrap_or(false);
                let ch = border_glyph(x, y, &occupied, self.win_cols, self.win_rows, active_rect);
                let mut cell = Cell::blank();
                cell.ch = ch;
                if near_active {
                    cell.attrs = Attr {
                        bold: true,
                        ..Attr::default()
                    };
                    cell.fg = Color::Indexed(6); // cyan
                } else {
                    cell.fg = Color::Indexed(8); // bright black / dim
                }
                if let Some(row) = self.composite.row_mut(y)
                    && x < row.len()
                {
                    row[x] = cell;
                    self.composite.mark_dirty(y);
                }
            }
        }
    }

    fn sync_active_cursor(&mut self) {
        if let Some(p) = self.panes.get(&self.active_pane) {
            self.composite.cursor_row = p.rect.y as usize + p.grid.cursor_row;
            self.composite.cursor_col = p.rect.x as usize + p.grid.cursor_col;
            self.composite.cursor_visible = p.grid.cursor_visible;
            self.composite.set_mouse_flags(p.grid.mouse_flags());
        }
    }
}

fn fill_grid_from_cells(grid: &mut Grid, cols: usize, cells: &[Cell]) {
    for (i, cell) in cells.iter().enumerate() {
        let row = i / cols.max(1);
        let col = i % cols.max(1);
        if row < grid.rows()
            && let Some(r) = grid.row_mut(row)
            && col < r.len()
        {
            r[col] = cell.clone();
        }
    }
}

fn apply_row(grid: &mut Grid, row: usize, cells: &[Cell]) {
    if let Some(r) = grid.row_mut(row) {
        for (c, cell) in cells.iter().enumerate() {
            if c < r.len() {
                r[c] = cell.clone();
            }
        }
        grid.mark_dirty(row);
    }
}

fn is_adjacent_to_rect(x: usize, y: usize, r: PaneRect) -> bool {
    let x0 = r.x as usize;
    let y0 = r.y as usize;
    let x1 = x0 + r.cols as usize;
    let y1 = y0 + r.rows as usize;
    // One cell outside the pane edges.
    let vert = x >= x0 && x < x1 && (y + 1 == y0 || y == y1);
    let horiz = y >= y0 && y < y1 && (x + 1 == x0 || x == x1);
    vert || horiz
}

/// Soft tint for inactive panes: only fill default backgrounds so apps
/// that set their own colours (vim, etc.) stay untouched.
fn composite_cell(src: &Cell, inactive: bool) -> Cell {
    let mut cell = src.clone();
    if inactive && matches!(cell.bg, Color::Default) {
        cell.bg = Color::Indexed(236);
    }
    cell
}

/// Glyph pointing toward the active pane when `active` is adjacent on a
/// straight edge; otherwise classic box-drawing.
fn border_glyph(
    x: usize,
    y: usize,
    occupied: &[bool],
    cols: usize,
    rows: usize,
    active: Option<PaneRect>,
) -> char {
    let at = |xx: isize, yy: isize| -> bool {
        if xx < 0 || yy < 0 || xx as usize >= cols || yy as usize >= rows {
            return false;
        }
        occupied[yy as usize * cols + xx as usize]
    };
    let up = y > 0 && !occupied[(y - 1) * cols + x];
    let down = y + 1 < rows && !occupied[(y + 1) * cols + x];
    let left = x > 0 && !occupied[y * cols + x - 1];
    let right = x + 1 < cols && !occupied[y * cols + x + 1];
    let pane_l = at(x as isize - 1, y as isize);
    let pane_r = at(x as isize + 1, y as isize);
    let pane_u = at(x as isize, y as isize - 1);
    let pane_d = at(x as isize, y as isize + 1);

    // A true junction has panes on both axes; gap-continuation along a
    // straight divider must not count as a cross (those stay directional).
    let cross = (pane_l || pane_r) && (pane_u || pane_d);
    if !cross
        && let Some(r) = active
        && let Some(ch) = direction_toward_active(x, y, r)
    {
        return ch;
    }

    match (
        pane_l || left,
        pane_r || right,
        pane_u || up,
        pane_d || down,
    ) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┴',
        (true, true, false, true) => '┬',
        (true, false, true, true) => '┤',
        (false, true, true, true) => '├',
        (true, true, _, _) => '─',
        (_, _, true, true) => '│',
        (true, false, true, false) => '┘',
        (true, false, false, true) => '┐',
        (false, true, true, false) => '└',
        (false, true, false, true) => '┌',
        _ if pane_l || pane_r => '│',
        _ => '─',
    }
}

/// `^v<>` pointing at `active` when this gap cell sits on one of its edges.
fn direction_toward_active(x: usize, y: usize, active: PaneRect) -> Option<char> {
    let x0 = active.x as usize;
    let y0 = active.y as usize;
    let x1 = x0 + active.cols as usize;
    let y1 = y0 + active.rows as usize;
    if x >= x0 && x < x1 && y + 1 == y0 {
        return Some('v'); // active is below → point down
    }
    if x >= x0 && x < x1 && y == y1 {
        return Some('^'); // active is above → point up
    }
    if y >= y0 && y < y1 && x + 1 == x0 {
        return Some('>'); // active is to the right → point right
    }
    if y >= y0 && y < y1 && x == x1 {
        return Some('<'); // active is to the left → point left
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_blit_places_panes() {
        let mut w = WindowView::single(24, 80);
        w.apply_layout(
            25,
            80,
            2,
            &[
                PaneRect {
                    pane_id: 1,
                    x: 0,
                    y: 0,
                    cols: 80,
                    rows: 12,
                },
                PaneRect {
                    pane_id: 2,
                    x: 0,
                    y: 13,
                    cols: 80,
                    rows: 12,
                },
            ],
        );
        assert_eq!(w.pane_count(), 2);
        assert_eq!(w.win_rows, 25);
        // Gap row 12 should be a border, not a pane cell.
        let gap = w.composite.cell(12, 0).unwrap();
        assert_ne!(gap.ch, '\0');
        assert_ne!(gap.ch, ' ');
    }

    #[test]
    fn active_border_points_at_focus_and_inactive_tints() {
        let mut w = WindowView::single(24, 80);
        w.apply_layout(
            25,
            80,
            2, // active = bottom pane
            &[
                PaneRect {
                    pane_id: 1,
                    x: 0,
                    y: 0,
                    cols: 80,
                    rows: 12,
                },
                PaneRect {
                    pane_id: 2,
                    x: 0,
                    y: 13,
                    cols: 80,
                    rows: 12,
                },
            ],
        );
        let gap = w.composite.cell(12, 5).unwrap();
        assert_eq!(gap.ch, 'v', "border should point down toward active");
        assert_eq!(gap.fg, Color::Indexed(6));

        // Inactive (top) default-bg cells get a soft tint; active do not.
        let top = w.composite.cell(0, 0).unwrap();
        let bot = w.composite.cell(13, 0).unwrap();
        assert_eq!(top.bg, Color::Indexed(236));
        assert_eq!(bot.bg, Color::Default);

        // Flip focus → arrows reverse, tint swaps.
        w.apply_layout(
            25,
            80,
            1,
            &[
                PaneRect {
                    pane_id: 1,
                    x: 0,
                    y: 0,
                    cols: 80,
                    rows: 12,
                },
                PaneRect {
                    pane_id: 2,
                    x: 0,
                    y: 13,
                    cols: 80,
                    rows: 12,
                },
            ],
        );
        assert_eq!(w.composite.cell(12, 5).unwrap().ch, '^');
        assert_eq!(w.composite.cell(0, 0).unwrap().bg, Color::Default);
        assert_eq!(w.composite.cell(13, 0).unwrap().bg, Color::Indexed(236));
    }

    #[test]
    fn horizontal_split_border_points_sideways() {
        let mut w = WindowView::single(24, 80);
        w.apply_layout(
            24,
            81,
            1, // active = left
            &[
                PaneRect {
                    pane_id: 1,
                    x: 0,
                    y: 0,
                    cols: 40,
                    rows: 24,
                },
                PaneRect {
                    pane_id: 2,
                    x: 41,
                    y: 0,
                    cols: 40,
                    rows: 24,
                },
            ],
        );
        assert_eq!(w.composite.cell(5, 40).unwrap().ch, '<');
        w.apply_layout(
            24,
            81,
            2,
            &[
                PaneRect {
                    pane_id: 1,
                    x: 0,
                    y: 0,
                    cols: 40,
                    rows: 24,
                },
                PaneRect {
                    pane_id: 2,
                    x: 41,
                    y: 0,
                    cols: 40,
                    rows: 24,
                },
            ],
        );
        assert_eq!(w.composite.cell(5, 40).unwrap().ch, '>');
    }
}
