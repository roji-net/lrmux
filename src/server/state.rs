// State persistence: save session/window state to disk for crash recovery.
//
// On startup, if a state file exists, it means the previous server crashed
// (or was killed) without a clean shutdown. The state file contains the
// session names, window names, child PIDs, and CWDs so the user can see
// what was running and potentially recover orphaned processes.

use std::path::Path;

use crate::server::session::Session;

/// State file path: same directory as the log file, with `.state` extension.
pub fn state_file_path(socket_path: &Path) -> std::path::PathBuf {
    let uid = unsafe { libc::getuid() };
    let server_name = socket_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    std::path::PathBuf::from(format!("/tmp/lrmux-{uid}/logs/{server_name}.state"))
}

/// Serialize the current sessions to the state file.
/// Called periodically during the event loop and on shutdown.
pub fn save_state(socket_path: &Path, sessions: &[Session]) {
    let path = state_file_path(socket_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut content = String::new();
    content.push_str("# lrmux server state (auto-generated)\n");
    content.push_str(&format!(
        "# saved at epoch {}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ));
    for session in sessions {
        content.push_str(&format!("[session:{}]\n", session.name));
        for w in &session.windows {
            let pid = w.pane.pty.child_pid.as_raw();
            let exited = w.pane.exited;
            let cwd = crate::pty::child_cwd_full(w.pane.pty.child_pid)
                .unwrap_or_else(|| "<unknown>".to_string());
            let status = if exited { "exited" } else { "running" };
            content.push_str(&format!(
                "  window name={} pid={} status={} cwd={}\n",
                w.name, pid, status, cwd
            ));
        }
    }
    let tmp = path.with_extension("state.tmp");
    if std::fs::write(&tmp, &content).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Check for a stale state file on startup and log a warning if found.
/// Returns the parsed state for display.
pub fn check_stale_state(socket_path: &Path) -> Option<String> {
    let path = state_file_path(socket_path);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    Some(content)
}

/// Remove the state file (called on graceful shutdown).
pub fn cleanup_state(socket_path: &Path) {
    let path = state_file_path(socket_path);
    let _ = std::fs::remove_file(&path);
}
