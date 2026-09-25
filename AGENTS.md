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

## Phase 4 status

- **Multiple sessions**: Server holds `Vec<Session>`, each session owns windows. Clients track their own `session_idx` + `active_window`.
- **Session commands**: `Ctrl-A C` (new session, uppercase), `Ctrl-A N` (next session), `Ctrl-A P` (previous session). Session names derived from CWD basename (e.g. `lrmux`), with `-2`, `-3` suffixes on collision.
- **Multiple windows**: `Ctrl-A c` (new window), `Ctrl-A n`/`Space` (next), `Ctrl-A p` (prev), `Ctrl-A 0-9` (select), `Ctrl-A x` (kill), `Ctrl-A k` (kill with y/n confirmation).
- **Kill session**: `Ctrl-A K` (kill current session with typed-name confirmation, shows window count).
- **Child exit behavior**: Exit codes 0, 130 (128+SIGINT, common when exiting shells with Ctrl-D after Ctrl-C), and -2 (direct SIGINT signal) auto-close the pane/window. Other non-zero exits keep the pane open with a red `[process exited, code N]` message so the user can read the output before closing with `Prefix x`. Signal exits show `[process exited, signal N]`.
- **Copy/scrollback mode**: `Ctrl-A [` enters vi-style copy mode. Navigate scrollback with `h`/`j`/`k`/`l`, `g`/`G` (top/bottom), `Ctrl-u`/`Ctrl-d` (half-page), `Ctrl-b`/`Ctrl-f` (full page), `0`/`^`/`$` (line start/end), arrow keys, Home/End, Page Up/Down. `Space` begins selection, `Enter` copies to internal paste buffer + system clipboard (auto-detects `pbcopy`/`xclip`/`wl-copy`). `q`/`Esc` quits. `Ctrl-A ]` pastes from internal buffer. Scrollback is synchronized from server to client via `ScrollbackUpdate` protocol messages. Cursor is visible in copy mode (shown with `\x1b[?25h`, hidden on exit).
- **Mouse wheel scrolling**: Reverted — not using mouse event interception. Instead, native terminal scrollback is enabled (see below).
- **Native terminal scrollback**: lrmux does NOT use the alternate screen (`\x1b[?1049h`). Instead, it renders on the main screen with a scroll region (`\x1b[1;<rows-1>r`) that excludes the status bar. When the grid scrolls, `\x1b[<N>S` is sent to scroll the terminal, pushing content into the terminal's native scrollback buffer. This allows the user to scroll up with the terminal's scrollbar or mouse wheel and see output from inside lrmux. This matches tmux's `smcup@:rmcup@` behavior. The internal scrollback is still maintained for copy mode.
- **Per-client views**: Each client has its own active session and active window. Grid updates are routed only to clients viewing the relevant window.
- **Status bar**: Blue background, session name in cyan, window list with active window highlighted in bold yellow. `*` marks the active window. Positioned at the bottom of the terminal.
- **Viewport model**: Canonical grid size set by the first client. SIGWINCH does NOT resize panes — the client renders a viewport (crop if smaller, filler if larger). `Ctrl-A F` sends an explicit canonical resize to the server. Filler region uses dim background with thin border lines.
- **Server lifecycle**: Server persists until all sessions are closed. Last window in a session removes the session. Last session shuts down the server.
- **Concurrent clients**: Multiple clients can connect to the same server simultaneously.
- **CLI commands**: `lrmux` (default: selector or auto-join), `lrmux <command> --help` for that command. `lrmux new-session [--headless]`, `lrmux new-server [-s name] [--tcp addr] [--headless]` (attaches unless `--headless`), `lrmux attach [host:port]`, `lrmux ls`, `lrmux list-servers`, `lrmux kill-server`, `lrmux -v`. `attach-session` is an alias of `attach`. `start-server` is an alias of `new-server --headless`. A `host:port` target connects over TCP instead of a local Unix socket.
- **Server discovery**: Scans `/tmp/lrmux-<UID>/` for socket files and probes each to find running servers.
- **Session queries**: `ListSessions` protocol message (lightweight: connect, query, disconnect). Server responds with `SessionList`.
- **Selector**: Interactive TUI with fuzzy filter, j/k navigation, Enter to join, n for new session, N for new server. `SelectSession` protocol message switches to the chosen session after handshake.
- **Not yet implemented**: Pane splits, layout engine, window auto-renumber (already works via Vec), search in copy mode, TOML config loading, F-key shortcuts.

## Crash recovery and state persistence

- **State file**: `/tmp/lrmux-<UID>/logs/<server-name>.state` — saved every ~10s and on shutdown. Contains session names, window names, child PIDs, CWDs, and process status.
- **Crash detection**: On startup, if a state file exists, the previous server crashed. lrmux logs a warning and prints the previous state to stderr.
- **Clean shutdown**: On graceful shutdown (all sessions closed, KillServer), the state file is removed. On error/panic shutdown, the state file is kept for crash analysis.
- **Panic handling**: The event loop is wrapped in `catch_unwind`. If the event loop panics, the panic is logged, state is saved, and the server exits with an error message instead of crashing silently.
- **SIGHUP on shutdown**: All living child processes receive SIGHUP before the server exits, so they can clean up instead of being orphaned.
- **Logging**: File log at `/tmp/lrmux-<UID>/logs/<server-name>.log`, optional remote syslog via `LRMUX_SYSLOG=host:port`, in-memory ring log (500 entries) viewable with `Ctrl-A \`.
- **Log levels**: `LRMUX_LOG_LEVEL=debug|info|warn|error` (default: info).

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:46cd31e7 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/core-concepts/sync-concepts.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   bd dolt push
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/core-concepts/sync-concepts.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->
