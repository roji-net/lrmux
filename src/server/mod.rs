// Server process: event loop, state, session/window/pane management.

mod event_loop;
mod pane;
mod session;
mod window;

use std::io;
use std::path::Path;

use crate::ipc;

/// Start the server: bind the socket, run the event loop.
pub fn run(socket_path: &Path) -> io::Result<()> {
    let listener = ipc::listen(socket_path)?;
    eprintln!("lrmux: server listening on {}", socket_path.display());
    event_loop::run(listener, socket_path)
}
