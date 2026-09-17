// Window: contains a single pane (for now), tracks window name.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::server::pane::Pane;

/// Global window ID counter (tmux uses @N format).
static WINDOW_ID: AtomicU32 = AtomicU32::new(1);

/// A window owns one pane (single-pane windows in Phase 4).
/// Multi-pane windows (splits) come later.
pub struct Window {
    pub id: u32,
    pub pane: Pane,
    pub name: String,
}

impl Window {
    pub fn new(rows: u16, cols: u16, name: String) -> Self {
        Self {
            id: WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            pane: Pane::new(rows, cols),
            name,
        }
    }

    pub fn new_in_cwd(rows: u16, cols: u16, name: String, cwd: &str) -> Self {
        Self {
            id: WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            pane: Pane::new_in_cwd(rows, cols, cwd),
            name,
        }
    }

    pub fn new_with_command(
        rows: u16,
        cols: u16,
        name: String,
        command: &str,
        cwd: Option<&str>,
    ) -> Self {
        Self {
            id: WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            pane: Pane::new_with_command(rows, cols, command, cwd),
            name,
        }
    }

    pub fn pty_fd(&self) -> i32 {
        self.pane.pty_fd()
    }

    /// Format the window ID as tmux-style: @N
    pub fn id_str(&self) -> String {
        format!("@{}", self.id)
    }
}
