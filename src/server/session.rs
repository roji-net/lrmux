// Session: a collection of windows; the unit of detach/attach.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::server::window::Window;

/// Global session ID counter (tmux uses $N format).
static SESSION_ID: AtomicU32 = AtomicU32::new(1);

/// Unix epoch seconds (wall-clock).
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
    /// Unix epoch seconds when this session was created.
    pub created_at: u64,
    /// Unix epoch seconds of the last PTY output in any window.
    pub last_activity: u64,
}

impl Session {
    pub fn new(name: String, grid_rows: u16, grid_cols: u16) -> Self {
        let window = Window::new(grid_rows, grid_cols, "shell".to_string());
        let now = unix_now();
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: vec![window],
            options: HashMap::new(),
            created_at: now,
            last_activity: now,
        }
    }

    pub fn new_in_cwd(name: String, grid_rows: u16, grid_cols: u16, cwd: &str) -> Self {
        let window = Window::new_in_cwd(grid_rows, grid_cols, "shell".to_string(), cwd);
        let now = unix_now();
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: vec![window],
            options: HashMap::new(),
            created_at: now,
            last_activity: now,
        }
    }

    /// First session/window runs `command` via `$SHELL -ci`
    /// (bootstrap for `new-server -- cmd` / `new-session` on a fresh server).
    pub fn new_with_command(
        name: String,
        grid_rows: u16,
        grid_cols: u16,
        command: &str,
        cwd: Option<&str>,
    ) -> Self {
        let window = Window::new_with_command(
            grid_rows,
            grid_cols,
            default_window_name_for_command(command),
            command,
            cwd,
        );
        let now = unix_now();
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: vec![window],
            options: HashMap::new(),
            created_at: now,
            last_activity: now,
        }
    }

    /// A session with no windows — used when iTerm2 affinities move a
    /// window into a new session (each OS window maps to a session).
    pub fn new_empty(name: String) -> Self {
        let now = unix_now();
        Self {
            id: SESSION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            windows: Vec::new(),
            options: HashMap::new(),
            created_at: now,
            last_activity: now,
        }
    }

    /// Format the session ID as tmux-style: $N
    pub fn id_str(&self) -> String {
        format!("${}", self.id)
    }

    /// True if any window has unseen activity.
    pub fn has_activity(&self) -> bool {
        self.windows.iter().any(|w| w.activity)
    }
}

fn default_window_name_for_command(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .and_then(|bin| std::path::Path::new(bin).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "shell".to_string())
}
