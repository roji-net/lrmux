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
    /// True when this window produced output while no interactive client
    /// was viewing it. Cleared when any client focuses the window.
    pub activity: bool,
    /// tmux `remain-on-exit`: keep the pane after the child exits on ANY
    /// exit code, instead of auto-closing on success codes.
    pub remain_on_exit: bool,
}

impl Window {
    /// `session_id` is the owning session's id — exported to the pane's
    /// child in the tmux-compat `TMUX` env var.
    pub fn new(rows: u16, cols: u16, name: String, session_id: u32) -> Self {
        Self {
            id: WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            pane: Pane::new(rows, cols, session_id),
            name,
            activity: false,
            remain_on_exit: false,
        }
    }

    pub fn new_in_cwd(rows: u16, cols: u16, name: String, cwd: &str, session_id: u32) -> Self {
        Self {
            id: WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            pane: Pane::new_in_cwd(rows, cols, cwd, session_id),
            name,
            activity: false,
            remain_on_exit: false,
        }
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
        Self {
            id: WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            pane: Pane::new_with_command(rows, cols, command, cwd, session_id, env),
            name,
            activity: false,
            remain_on_exit: false,
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
