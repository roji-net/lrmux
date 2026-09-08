# lrmux

A modern, fast, minimal-dependency terminal multiplexer written in Rust.

## Build commands

```sh
cargo build              # debug build
cargo build --release    # release build (LTO enabled)
cargo fmt                # format code
cargo fmt --check        # check formatting without modifying
cargo clippy             # lint
cargo clippy -- -D warnings  # lint, treat warnings as errors
cargo test               # run tests
```

## Project structure

See `docs/DESIGN.md` for the full design document.

Module layout (per §5.7 of the design doc):
- `src/main.rs` — entry, arg parsing, client/server fork decision
- `src/server/` — server process: event loop, state, session/window/pane mgmt
- `src/client/` — client process: raw mode, input relay, output render
- `src/pty/` — PTY spawn/resize/read/write over nix
- `src/term/` — termios raw mode, escape-sequence writer, truecolor SGR
- `src/vt/` — VT parser (vte wrapper) → grid updates
- `src/grid/` — screen grid + scrollback ring buffer, cell/attr types
- `src/proto/` — client↔server message protocol (encode/decode)
- `src/ipc/` — Unix socket transport (abstracted for future TCP)
- `src/config/` — TOML config loading + keybinding map
- `src/keys/` — key input parsing, prefix detection, command dispatch
- `src/statusbar/` — status line rendering
- `src/layout/` — pane split/resize layout algorithms

## Conventions

- Minimal dependencies: `nix` + `libc` for the system layer, `vte` for VT parsing, `toml` + `serde` for config. No async runtime (manual poll/kqueue/epoll event loop).
- Passthrough invariant: lrmux intercepts only the prefix key (default Ctrl-A) and optionally F-key shortcuts. Everything else — keyboard and mouse — passes through to the child process untouched.
- Rust edition 2024.
- Target platforms: macOS + Linux (v1).
