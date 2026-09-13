// Server process: event loop, state, session/window/pane management.

mod event_loop;
mod pane;
mod session;
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

    log::info(&format!("server starting on {}", socket_path.display()));
    eprintln!("lrmux: server listening on {}", socket_path.display());

    let result = event_loop::run(listener, socket_path);

    log::info("server stopped.");
    result
}
