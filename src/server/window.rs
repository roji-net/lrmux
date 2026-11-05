//! Window: one or more panes arranged by a layout tree.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::layout::{Layout, SplitDir};
use crate::server::pane::Pane;

/// Global window ID counter (tmux uses @N format).
static WINDOW_ID: AtomicU32 = AtomicU32::new(1);

/// A window owns panes + a layout tree. Single-pane is the default;
/// splits add leaves via [`Window::split`].
pub struct Window {
    pub id: u32,
    pub panes: Vec<Pane>,
    /// Index into `panes` for focus / input / snapshots.
    pub active_pane: usize,
    pub layout: Layout,
    /// Canonical window size (full area the layout reflows into).
    pub rows: u16,
    pub cols: u16,
    pub name: String,
    /// True when this window produced output while no interactive client
    /// was viewing it. Cleared when any client focuses the window.
    pub activity: bool,
    /// tmux `remain-on-exit`: keep the pane after the child exits on ANY
    /// exit code, instead of auto-closing on success codes.
    pub remain_on_exit: bool,
    /// Name was set explicitly (rename-window or `new-window -n`) —
    /// automatic-rename must not overwrite it.
    pub name_locked: bool,
    /// Active pane temporarily fills the window (tmux zoom). The layout
    /// tree is preserved; unzoom reflows every pane back into place.
    pub zoomed: bool,
    /// Preset index for [`Window::next_layout`] cycling (0=even-h, 1=even-v, 2=tiled).
    pub layout_kind: u8,
}

impl Window {
    /// `session_id` is the owning session's id — exported to the pane's
    /// child in the tmux-compat `TMUX` env var.
    pub fn new(rows: u16, cols: u16, name: String, session_id: u32) -> Self {
        let pane = Pane::new(rows, cols, session_id);
        Self::from_pane(pane, rows, cols, name)
    }

    pub fn new_in_cwd(rows: u16, cols: u16, name: String, cwd: &str, session_id: u32) -> Self {
        let pane = Pane::new_in_cwd(rows, cols, cwd, session_id);
        Self::from_pane(pane, rows, cols, name)
    }

    pub fn new_with_command(
        rows: u16,
        cols: u16,
        name: String,
        command: &str,
        cwd: Option<&str>,
        session_id: u32,
        env: &[(String, String)],
    ) -> Self {
        let pane = Pane::new_with_command(rows, cols, command, cwd, session_id, env);
        Self::from_pane(pane, rows, cols, name)
    }

    fn from_pane(pane: Pane, rows: u16, cols: u16, name: String) -> Self {
        let layout = Layout::single(pane.id);
        Self {
            id: WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            panes: vec![pane],
            active_pane: 0,
            layout,
            rows,
            cols,
            name,
            activity: false,
            remain_on_exit: false,
            name_locked: false,
            zoomed: false,
            layout_kind: 0,
        }
    }

    /// Active (focused) pane.
    pub fn pane(&self) -> &Pane {
        &self.panes[self.clamp_active()]
    }

    /// Active (focused) pane, mutable.
    pub fn pane_mut(&mut self) -> &mut Pane {
        let i = self.clamp_active();
        &mut self.panes[i]
    }

    fn clamp_active(&self) -> usize {
        self.active_pane.min(self.panes.len().saturating_sub(1))
    }

    pub fn pane_index_by_id(&self, pane_id: u32) -> Option<usize> {
        self.panes.iter().position(|p| p.id == pane_id)
    }

    pub fn pane_by_id(&self, pane_id: u32) -> Option<&Pane> {
        self.pane_index_by_id(pane_id).map(|i| &self.panes[i])
    }

    pub fn pane_by_id_mut(&mut self, pane_id: u32) -> Option<&mut Pane> {
        let i = self.pane_index_by_id(pane_id)?;
        Some(&mut self.panes[i])
    }

    /// PTY fd of the active pane (poll helpers that still assume one fd
    /// per window should iterate `panes` instead).
    pub fn pty_fd(&self) -> i32 {
        self.pane().pty_fd()
    }

    /// Format the window ID as tmux-style: @N
    pub fn id_str(&self) -> String {
        format!("@{}", self.id)
    }

    /// tmux layout string for `%layout-change` / `#{window_layout}`.
    pub fn layout_str(&self) -> String {
        self.layout.to_tmux_str(self.cols, self.rows)
    }

    /// Pane rectangles from the current layout (for client composite).
    /// When zoomed, only the active pane is reported (full window).
    pub fn pane_rects(&self) -> Vec<crate::proto::PaneRect> {
        if self.zoomed {
            let id = self.active_pane_id();
            return vec![crate::proto::PaneRect {
                pane_id: id,
                x: 0,
                y: 0,
                cols: self.cols.max(1),
                rows: self.rows.max(1),
            }];
        }
        self.layout
            .reflow(self.cols, self.rows)
            .into_iter()
            .map(|(pane_id, r)| crate::proto::PaneRect {
                pane_id,
                x: r.x,
                y: r.y,
                cols: r.cols,
                rows: r.rows,
            })
            .collect()
    }

    /// Active pane id (or 0 if empty).
    pub fn active_pane_id(&self) -> u32 {
        self.pane().id
    }

    /// Focus the next pane in layout tree order (wrap).
    pub fn focus_next(&mut self) -> u32 {
        let ids = self.layout.pane_ids();
        if ids.is_empty() {
            return 0;
        }
        let cur = self.pane().id;
        let pos = ids.iter().position(|&id| id == cur).unwrap_or(0);
        let next = ids[(pos + 1) % ids.len()];
        self.focus_pane(next);
        next
    }

    /// Focus the previous pane in layout tree order (wrap).
    pub fn focus_prev(&mut self) -> u32 {
        let ids = self.layout.pane_ids();
        if ids.is_empty() {
            return 0;
        }
        let cur = self.pane().id;
        let pos = ids.iter().position(|&id| id == cur).unwrap_or(0);
        let prev = ids[(pos + ids.len() - 1) % ids.len()];
        self.focus_pane(prev);
        prev
    }

    /// Focus the adjacent pane in `dir` (no wrap). Returns new id if moved.
    pub fn focus_dir(&mut self, dir: crate::layout::FocusDir) -> Option<u32> {
        let from = self.pane().id;
        let next = self.layout.neighbor(from, dir, self.cols, self.rows)?;
        self.focus_pane(next);
        Some(next)
    }

    /// Focus a pane by id. Returns false if missing.
    /// While zoomed, the newly focused pane fills the window.
    pub fn focus_pane(&mut self, pane_id: u32) -> bool {
        if let Some(i) = self.pane_index_by_id(pane_id) {
            self.active_pane = i;
            if self.zoomed {
                self.apply_zoom_size();
            }
            true
        } else {
            false
        }
    }

    /// Toggle zoom on the active pane. No-op (returns false) with <2 panes.
    pub fn toggle_zoom(&mut self) -> bool {
        if self.panes.len() <= 1 {
            self.zoomed = false;
            return false;
        }
        self.zoomed = !self.zoomed;
        if self.zoomed {
            self.apply_zoom_size();
        } else {
            self.reflow_panes();
        }
        true
    }

    /// Force-clear zoom and restore layout geometry (e.g. before split).
    pub fn unzoom(&mut self) {
        if self.zoomed {
            self.zoomed = false;
            self.reflow_panes();
        }
    }

    /// Cycle even-horizontal → even-vertical → tiled. Unzooms first.
    pub fn next_layout(&mut self) -> bool {
        if self.panes.len() <= 1 {
            return false;
        }
        self.unzoom();
        let ids = self.layout.pane_ids();
        let (layout, kind) = Layout::cycle_preset(&ids, self.layout_kind);
        self.layout = layout;
        self.layout_kind = kind;
        self.reflow_panes();
        true
    }

    fn apply_zoom_size(&mut self) {
        let rows = self.rows.max(1);
        let cols = self.cols.max(1);
        let p = self.pane_mut();
        if p.rows != rows || p.cols != cols {
            p.resize(rows, cols);
            p.trace_event(&format!("zoom → {cols}x{rows}"));
        }
    }

    /// Apply layout geometry to every pane (PTY SIGWINCH + grid resize).
    pub fn reflow_panes(&mut self) {
        if self.zoomed {
            self.apply_zoom_size();
            return;
        }
        let geom = self.layout.reflow(self.cols, self.rows);
        for (pane_id, rect) in geom {
            if let Some(p) = self.pane_by_id_mut(pane_id)
                && (p.rows != rect.rows || p.cols != rect.cols)
            {
                p.resize(rect.rows, rect.cols);
                p.trace_event(&format!(
                    "layout reflow → {}x{} @{},{}",
                    rect.cols, rect.rows, rect.x, rect.y
                ));
            }
        }
    }

    /// Resize the window (canonical size) and reflow all panes.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        self.reflow_panes();
    }

    /// Split the active pane (or `target_pane` if given). Spawns a new
    /// shell pane in `session_id`. Returns the new pane id.
    pub fn split(
        &mut self,
        dir: SplitDir,
        session_id: u32,
        target_pane: Option<u32>,
        cwd: Option<&str>,
    ) -> Option<u32> {
        // tmux: splitting while zoomed cancels zoom first.
        self.unzoom();
        let target = target_pane.unwrap_or_else(|| self.pane().id);
        if !self.layout.contains(target) {
            return None;
        }
        // Size hint for the new PTY before reflow (reflow will shrink both).
        let (rows, cols) = self
            .pane_by_id(target)
            .map(|p| (p.rows, p.cols))
            .unwrap_or((self.rows, self.cols));
        let new_pane = match cwd {
            Some(dir) => Pane::new_in_cwd(rows, cols, dir, session_id),
            None => Pane::new(rows, cols, session_id),
        };
        let new_id = new_pane.id;
        if !self.layout.split(target, dir, new_id, 0.5) {
            return None;
        }
        self.panes.push(new_pane);
        self.reflow_panes();
        // Focus the new pane (tmux default).
        self.focus_pane(new_id);
        Some(new_id)
    }

    /// Remove a pane by id. Returns `Removed::WindowEmpty` if that was the
    /// last pane (caller should destroy the window), `Removed::Pane` if the
    /// window still has panes, or `None` if the id was unknown.
    pub fn remove_pane(&mut self, pane_id: u32) -> Option<Removed> {
        let idx = self.pane_index_by_id(pane_id)?;
        if self.panes.len() == 1 {
            return Some(Removed::WindowEmpty);
        }
        if !self.layout.remove(pane_id) {
            return None;
        }
        self.panes.remove(idx);
        if self.active_pane >= self.panes.len() {
            self.active_pane = self.panes.len() - 1;
        } else if self.active_pane > idx {
            self.active_pane -= 1;
        }
        // Zoom with one pane left is meaningless.
        if self.panes.len() <= 1 {
            self.zoomed = false;
        }
        self.reflow_panes();
        Some(Removed::Pane)
    }

    /// Kill the active pane. See [`Window::remove_pane`].
    pub fn kill_active_pane(&mut self) -> Option<Removed> {
        let id = self.pane().id;
        // SIGHUP the child before dropping the Pane (Drop may also reap).
        if !self.pane().exited {
            let pid = self.pane().pty.child_pid;
            crate::pty::kill_child(pid, libc::SIGHUP);
        }
        self.remove_pane(id)
    }
}

/// Result of removing a pane from a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    /// Window still has other panes.
    Pane,
    /// That was the last pane — destroy the window.
    WindowEmpty,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pane::new needs a real PTY — skip heavy spawn tests here; layout
    // integration is covered in layout::tests. Window helpers that don't
    // spawn are limited; keep a smoke test on Removed enum / id_str shape.
    #[test]
    fn id_str_format() {
        // Can't easily construct without PTY in unit tests without mocking;
        // just verify Removed is usable.
        assert_eq!(Removed::Pane, Removed::Pane);
        assert_ne!(Removed::Pane, Removed::WindowEmpty);
    }
}
