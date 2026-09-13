// Server process: event loop, state, session/window/pane management.

mod event_loop;
mod pane;
mod session;
mod state;
mod window;

use std::io;
use std::path::Path;

use crate::ipc;
use crate::log;

/// Start the server: bind the socket, run the event loop.
pub fn run(socket_path: &Path) -> io::Result<()> {
    let listener = ipc::listen(socket_path)?;

    // Initialize logging.
    let uid = unsafe { libc::getuid() };
    let log_dir = format!("/tmp/lrmux-{uid}/logs");
    let server_name = socket_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    let syslog = std::env::var("LRMUX_SYSLOG").ok().and_then(|s| {
        let parts: Vec<&str> = s.rsplitn(2, ':').collect();
        if parts.len() == 2 {
            Some((parts[1].to_string(), parts[0].parse::<u16>().ok()?))
        } else {
            None
        }
    });
    let log_level = match std::env::var("LRMUX_LOG_LEVEL").as_deref() {
        Ok("debug") => log::Level::Debug,
        Ok("warn") => log::Level::Warn,
        Ok("error") => log::Level::Error,
        _ => log::Level::Info,
    };
    log::init(&log_dir, server_name, log_level, syslog);
    log::install_panic_hook();

    // Check for stale state file (indicates a previous crash).
    if let Some(stale_state) = state::check_stale_state(socket_path) {
        log::warn("previous server did not shut down cleanly — stale state file found");
        log::warn(&format!("previous state:\n{stale_state}"));
        eprintln!(
            "lrmux: WARNING — previous server may have crashed. Previous state:\n{stale_state}"
        );
        eprintln!("lrmux: check log file at {log_dir}/{server_name}.log for details.");
    }

    log::info(&format!("server starting on {}", socket_path.display()));
    eprintln!("lrmux: server listening on {}", socket_path.display());

    // Wrap the event loop in catch_unwind so a panic doesn't kill the
    // server process without cleanup. The panic hook logs the panic.
    // If the event loop panics, we log it and exit with an error
    // (state file is kept for crash analysis).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        event_loop::run(listener, socket_path)
    }));

    match result {
        Ok(inner) => {
            if inner.is_ok() {
                // Clean shutdown — remove state file.
                state::cleanup_state(socket_path);
                log::info("server stopped cleanly.");
            } else {
                // Error shutdown — keep state file for crash analysis.
                log::error(&format!(
                    "server stopped with error: {:?}",
                    inner.as_ref().err()
                ));
            }
            inner
        }
        Err(_) => {
            // Panic caught — log and exit with error.
            log::error("PANIC in event loop, exiting");
            eprintln!("lrmux: PANIC in event loop. State saved. Check log for details.");
            // State file is kept for crash analysis (not cleaned up).
            Err(io::Error::other("event loop panic"))
        }
    }
}
