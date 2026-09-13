// Session: a collection of windows; the unit of detach/attach.

use crate::server::window::Window;

/// A session owns a list of windows and has a name.
/// Multiple clients can be attached to the same session.
pub struct Session {
    pub name: String,
    pub windows: Vec<Window>,
}

impl Session {
    pub fn new(name: String, grid_rows: u16, grid_cols: u16) -> Self {
        let window = Window::new(grid_rows, grid_cols, "shell".to_string());
        Self {
            name,
            windows: vec![window],
        }
    }

    pub fn new_in_cwd(name: String, grid_rows: u16, grid_cols: u16, cwd: &str) -> Self {
        let window = Window::new_in_cwd(grid_rows, grid_cols, "shell".to_string(), cwd);
        Self {
            name,
            windows: vec![window],
        }
    }
}
