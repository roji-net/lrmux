// lrmux — a modern, fast, minimal-dependency terminal multiplexer

// Phase 0: stubs are not yet wired up. Remove this allow as modules are implemented.
#![allow(dead_code)]

mod client;
mod config;
mod grid;
mod ipc;
mod keys;
mod layout;
mod proto;
mod pty;
mod server;
mod statusbar;
mod term;
mod vt;

fn main() {
    // Phase 0: stub entry point.
    // Phase 1+: arg parsing, client/server fork decision, startup selector.
    println!("lrmux v{} — not yet functional", env!("CARGO_PKG_VERSION"));
}
