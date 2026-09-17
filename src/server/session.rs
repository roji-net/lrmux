// Session: a collection of windows; the unit of detach/attach.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::server::window::Window;

/// Global session ID counter (tmux uses $N format).
static SESSION_ID: AtomicU32 = AtomicU32::new(1);

/// A session owns a list of windows and has a name.
/// Multiple clients can be attached to the same session.
pub struct Session {
    pub id: u32,
    pub name: String,
    pub windows: Vec<Window>,
}

impl Session {
    pub fn new(name: String, grid_rows: u16, grid_cols: u16) -> Self {
        let window = Window::new(grid_rows, grid_cols, "shell".to_string());
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: vec![window],
        }
    }

    pub fn new_in_cwd(name: String, grid_rows: u16, grid_cols: u16, cwd: &str) -> Self {
        let window = Window::new_in_cwd(grid_rows, grid_cols, "shell".to_string(), cwd);
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: vec![window],
        }
    }

    /// Format the session ID as tmux-style: $N
    pub fn id_str(&self) -> String {
        format!("${}", self.id)
    }
}
