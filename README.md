# lrmux

A modern, fast, minimal-dependency terminal multiplexer written in Rust.

Inspired by [byobu](https://www.byobu.org/)’s ergonomics and [tmux](https://github.com/tmux/tmux)’s client–server model, rebuilt for truecolor terminals and AI-friendly workflows.

## Why lrmux?

**Passthrough by default.** lrmux intercepts only the prefix key (default `Ctrl-A`) and a small set of optional shortcuts. Everything else — keyboard and mouse — goes straight to the child process untouched.

That matters for AI CLIs (Cursor, Claude Code, Devin, Aider, …), editors, and TUIs whose keybindings collide with traditional multiplexers. If you don’t press the prefix, lrmux is invisible to the program inside.

Other goals:

- **Truecolor first-class** — 24-bit color passes through faithfully
- **Minimal dependencies** — `nix` / `libc`, `vte`, `toml` + `serde`; no async runtime
- **Single binary** — one `lrmux` acts as both client and server
- **iTerm2 integration** — `lrmux -CC` speaks tmux control mode

## Features

| Area | Status |
|------|--------|
| Multiple servers & sessions | ✓ |
| Multiple windows per session | ✓ |
| Detach / reattach | ✓ |
| Status bar | ✓ |
| Copy / scrollback mode (`Ctrl-A [`) | ✓ |
| Interactive session selector | ✓ |
| iTerm2 control mode (`-CC`) | ✓ |
| Native terminal scrollback | ✓ (no alternate screen) |
| Pane splits / layouts | Not yet |

## Install

```sh
cargo install --path .
```

Requires a recent Rust toolchain (edition 2024).

## Quick start

```sh
lrmux                  # selector, or auto-join if there’s only one session
lrmux new-session      # create a session (name from cwd by default)
lrmux -CC              # iTerm2 tmux integration
lrmux ls               # list sessions
lrmux kill-server      # shut down the default server
```

### Prefix commands (`Ctrl-A` then …)

| Key | Action |
|-----|--------|
| `c` | New window |
| `n` / `p` | Next / previous window |
| `0`–`9` | Select window |
| `C` | New session |
| `N` / `P` | Next / previous session |
| `[` / `]` | Copy mode / paste |
| `d` | Detach |
| `x` | Kill window |
| `K` | Kill session |
| `F` | Resize grid to terminal |
| `?` | Keybindings help |
| `\` | Server log |

Full CLI help: `lrmux --help`.

## Architecture (short)

```
Server  →  Session  →  Window  →  Pane (PTY + grid + scrollback)
```

The server owns PTYs and terminal state. Clients attach over a Unix socket (`/tmp/lrmux-<UID>/<server>`), render a local copy of the grid, and relay input. Multiple clients can attach to the same server.

Logs live under `/tmp/lrmux-<UID>/logs/<server>.log`.

## Build & develop

```sh
cargo build --release
cargo fmt
cargo clippy -- -D warnings
cargo test
```

Design details: [`docs/DESIGN.md`](docs/DESIGN.md).

## License

MIT
