// Window: contains a single pane (for now), tracks window name.

use crate::server::pane::Pane;

/// A window owns one pane (single-pane windows in Phase 4).
/// Multi-pane windows (splits) come later.
pub struct Window {
    pub pane: Pane,
    pub name: String,
}

impl Window {
    pub fn new(rows: u16, cols: u16, name: String) -> Self {
        Self {
            pane: Pane::new(rows, cols),
            name,
        }
    }

    pub fn new_with_command(rows: u16, cols: u16, name: String, command: &str) -> Self {
        Self {
            pane: Pane::new_with_command(rows, cols, command),
            name,
        }
    }

    pub fn pty_fd(&self) -> i32 {
        self.pane.pty_fd()
    }
}
