# lrmux

A modern, fast, minimal-dependency terminal multiplexer written in Rust.

Inspired by [byobu](https://www.byobu.org/)’s ergonomics and [tmux](https://github.com/tmux/tmux)’s client–server model, rebuilt for truecolor terminals and AI-friendly workflows.

## Why lrmux?

**Passthrough by default.** lrmux intercepts only the prefix key (default `Ctrl-A`) and a small set of optional shortcuts. Everything else — keyboard and mouse — goes straight to the child process untouched.

That matters for AI CLIs (Cursor, Claude Code, Devin, Aider, …), editors, and TUIs whose keybindings collide with traditional multiplexers. If you don’t press the prefix, lrmux is invisible to the program inside.

Other goals:

- **Truecolor first-class** — 24-bit color passes through faithfully
- **Minimal dependencies** — `nix` / `libc`, `vte`, `toml` + `serde`, `rustls` (+ pem/rcgen) for optional TLS; no async runtime
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
| TCP attach + UDP discovery | ✓ (off by default; see Remote) |
| TLS + PSK for TCP | ✓ (`tls = auto`, `safe_networks`, `psk`) |
| WebSocket + browser client | ✓ (`ws_listen` / `--ws`, `web/`) |
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
| `,` | Network / PSK setup |

Full CLI help: `lrmux --help`.

## Architecture (short)

```
Server  →  Session  →  Window  →  Pane (PTY + grid + scrollback)
```

The server owns PTYs and terminal state. Clients attach over a Unix socket (`/tmp/lrmux-<UID>/<server>`), or over TCP when configured. They render a local copy of the grid and relay input. Multiple clients can attach to the same server.

Logs live under `/tmp/lrmux-<UID>/logs/<server>.log`.

## Remote connectivity

All network features are **off by default**. Configure `~/.config/lrmux/config.toml`:

```toml
[network]
tcp_listen = "0.0.0.0:17280"   # empty = no TCP listener
ws_listen = "127.0.0.1:17282"  # empty = no WebSocket listener (browser client)
discovery = true                 # optional; also auto-enabled when TCP/WS is on
discovery_port = 17280
tls = "auto"                     # off | on | auto (TCP only; use a proxy for WSS)
# safe_networks is empty by default (safe): under tls=auto every TCP peer
# requires TLS. Add CIDRs only where you deliberately allow plaintext, e.g.:
# safe_networks = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "127.0.0.0/8"]
psk = ""                         # shared secret for TCP/WS Identify
# auth_token = ""                # deprecated alias for psk
# tls_cert / tls_key             # optional; auto-generated under ~/.config/lrmux/certs/
```

### Typical remote flow (share a PSK, not config files)

```sh
# Host with the server
lrmux start-server -s remote --tcp 0.0.0.0:17280
lrmux psk generate               # prints the secret once; saves to config
# or inside a session: Ctrl-A , → [g]enerate PSK

# Other machine (LAN: use ls / selector; else pass host:port)
lrmux --tcp 192.168.1.10:17280 --psk 'THE_SECRET'
```

### Browser client (lrmux-web)

Same binary protocol over WebSocket. Bind a WS listener and open `web/`:

```sh
lrmux start-server -s web --ws 127.0.0.1:17282
python3 -m http.server -d web 8080
# browser → http://127.0.0.1:8080 — connect to ws://127.0.0.1:17282
# set the same PSK if the server has one
```

For remote/HTTPS, terminate TLS at a reverse proxy and expose `wss://`. A Rust/WASM
client can reuse this transport later; the MVP is plain JS in `web/`.

`lrmux ls`, `ls-servers`, `discover`, and the session selector share one inventory
(local Unix sockets + LAN UDP discovery). LAN rows are tagged `[lan]`.

Unix sockets stay FS-permission gated (no PSK). TCP uses the PSK for auth and
TLS for encryption when required by `tls` / `safe_networks`. WebSocket uses the
same PSK; encrypt with WSS via a proxy.

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
