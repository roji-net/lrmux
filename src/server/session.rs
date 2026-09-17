// Session: a collection of windows; the unit of detach/attach.

use std::collections::HashMap;
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
    /// User options (@name) set via `set -t $N @k v` — iTerm2 stores
    /// @affinities, @hidden, @tab_colors etc. here to reconstruct tab
    /// groupings across reattach.
    pub options: HashMap<String, String>,
}

impl Session {
    pub fn new(name: String, grid_rows: u16, grid_cols: u16) -> Self {
        let window = Window::new(grid_rows, grid_cols, "shell".to_string());
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: vec![window],
            options: HashMap::new(),
        }
    }

    pub fn new_in_cwd(name: String, grid_rows: u16, grid_cols: u16, cwd: &str) -> Self {
        let window = Window::new_in_cwd(grid_rows, grid_cols, "shell".to_string(), cwd);
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: vec![window],
            options: HashMap::new(),
        }
    }

    /// A session with no windows — used when iTerm2 affinities move a
    /// window into a new session (each OS window maps to a session).
    pub fn new_empty(name: String) -> Self {
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: Vec::new(),
            options: HashMap::new(),
        }
    }

    /// Format the session ID as tmux-style: $N
    pub fn id_str(&self) -> String {
        format!("${}", self.id)
    }
}
