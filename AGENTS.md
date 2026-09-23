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

## Known issues (Phase 2)

- **Cursor jump on Enter (cosmetic)**: When pressing Enter in zsh, the cursor briefly appears to jump to the end of the line before settling at the new prompt position. This is because the diff renderer writes changed cells sequentially (cursor follows along) and then repositions. The cursor-hide/show around render helps but doesn't fully eliminate the effect. Potential fixes to explore later: (a) batch all cursor movements and only emit one final positioning, (b) track the grid cursor more aggressively so the renderer knows the final position before writing, (c) skip repositioning when the cursor is already at the right spot after sequential writes.
- **No unit tests yet**: `cargo test` reports 0 tests. Need tests for grid printing/wrapping, scrollback ring, SGR parsing, renderer output, protocol encode/decode.
- **CSI private mode handling**: The VT parser checks `intermediates.contains(b'?')` but in `vte` the private marker may be in the parameter structure, not intermediates. Needs validation.
- **No SIGWINCH handling**: Terminal resize is not propagated to the PTY or grid at runtime.

## Phase 3 status

- **Server/client split implemented**: The binary forks a server process (child) on first use, then connects as a client over a Unix socket at `/tmp/lrmux-<UID>/default`.
- **Protocol**: Length-prefixed binary messages (4-byte LE u32 length + 1-byte type + payload). Client→Server: Identify, PaneInput, Resize, Detach. Server→Client: IdentifyAck, GridSnapshot, GridUpdate, PaneExit, Error.
- **Architecture**: Server owns PTY + grid + VT parser. After each PTY read, sends dirty rows + cursor to client. Client maintains its own grid copy, applies updates, and renders locally using the diff-based renderer.
- **Single session/window/pane**: Phase 3 proves the architecture with one session, one window, one pane. Multi-pane/multi-window is deferred.
- **Server lifecycle**: Server exits when the client disconnects or the child process exits. Socket file is cleaned up on exit.
- **No reconnect yet**: If the client disconnects, the server exits. Reconnect to a persistent server is deferred.
- **No SIGWINCH yet**: Resize messages are handled in the protocol but not triggered by signal handling.
