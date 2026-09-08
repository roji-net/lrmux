// Server process: event loop, state, session/window/pane management.

mod event_loop;
mod pane;
mod session;
mod window;

pub fn run() {
    // Phase 3+: fork server, bind socket, run event loop.
    todo!("server::run");
}
