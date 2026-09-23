# lrmux — Design Document

> A modern, fast, minimal-dependency terminal multiplexer written in Rust, inspired by byobu's usability and tmux's architecture, reimagined for truecolor terminals and AI-friendly workflows.

---

## 1. Overview & Philosophy

### What is lrmux?

`lrmux` is a terminal multiplexer built from scratch in Rust. It takes the usability model of [byobu](https://www.byobu.org/) (F-key shortcuts, status bar, sensible defaults) and the proven architecture of [tmux](https://github.com/tmux/tmux) (client-server model, sessions/windows/panes), and rebuilds it with a modern foundation:

- **Truecolor (24-bit) first-class support** — child programs see a truecolor-capable terminal and their 24-bit output is passed through faithfully.
- **AI-CLI-friendly by default** — the core design invariant.
- **Minimal dependencies** — lean on `nix`/`libc` for the system layer; hand-roll the rest where it keeps control and reduces supply-chain surface.
- **Single binary** — one `lrmux` executable acts as both client and server (the tmux model).

### Core philosophy: passthrough everything except the prefix key

This is the defining principle of lrmux and what sets it apart from tmux and byobu.

> **Invariant:** lrmux intercepts *only* the configured prefix key (default `Ctrl-A`) and, optionally, a small set of F-key shortcuts that the user can disable. Every other keystroke — and all mouse input — flows to the child process in the active pane, unmodified and untouched.

Why this matters:

- **tmux and byobu intercept many keys** — prefix commands, copy-mode keys, mouse events, status-bar interactions, and various global bindings. Programs running inside (editors, REPLs, AI CLIs) frequently have their own keybindings collide with the multiplexer's. lrmux avoids this by intercepting only the prefix.
- **AI CLI tools** (Devin, Claude Code, Cursor, Aider, etc.) are especially sensitive: they rely on rich keybindings, function keys, and modifier combos. A multiplexer that swallows those keys breaks the AI tool's UX.
- **lrmux's contract**: if you don't press the prefix, lrmux is invisible to the child. This makes it safe to run any AI CLI, any editor, any TUI, without configuring passthrough rules.

The prefix key is configurable (`Ctrl-A` by default, matching byobu/screen convention). F-key shortcuts are enabled by default for byobu-like ergonomics but can be disabled entirely in config so they too pass through.

### Inspiration

| Source | What we take |
|--------|--------------|
| byobu | F-key shortcuts, status bar, `Ctrl-A` prefix, sensible defaults, "just works" ergonomics |
| tmux | Client-server architecture, sessions/windows/panes hierarchy, splits, detach/reattach, single binary |
| Modern terminals | Truecolor, modern escape sequences, clean rendering |

---

## 2. Functional Requirements

### 2.1 Server & session model

- **One server, many sessions**: a single lrmux server process can host multiple sessions simultaneously. Sessions persist on the server after the client detaches.
- **Many servers, concurrently**: multiple lrmux servers can run at the same time, each identified by a name and listening on its own Unix socket. This lets users separate work contexts (e.g. a `work` server and a `personal` server).
- **Single binary, dual role**: the `lrmux` binary is both client and server. When invoked, it tries to connect to an existing server socket; if none exists and a session is requested, it forks a server process and reconnects (the tmux model).

### 2.2 Startup selector

Running `lrmux` with no arguments shows an **interactive selector**:

1. List all running lrmux servers (by name).
2. For each server, list its sessions (with attached-client count, window count, creation time).
3. Let the user pick a server + session to join, or create a new session on a chosen server, or start a brand-new server.

This is the primary entry point for day-to-day use — the user never needs to remember server/session names.

### 2.3 Hierarchy

lrmux uses tmux's proven four-level hierarchy:

```
Server
└── Session
    └── Window
        └── Pane (a PTY running a shell/program)
```

- **Server**: the persistent daemon owning all state.
- **Session**: a collection of windows; the unit of detach/attach. Each session has its own current window and options.
- **Window**: occupies the full client terminal; contains one or more panes arranged in a layout.
- **Pane**: a rectangular region with a PTY running a child process, plus a grid (visible content + scrollback).

### 2.4 Splits

- Horizontal and vertical pane splits within a window.
- Pane focus cycling, pane resizing, pane closing.
- A layout engine manages pane geometry. Layouts reflow only on explicit resize (see §2.5), not on every client connect.

### 2.5 Detachment, reattachment & the viewport model

- Detaching leaves the session running on the server (child processes keep running).
- Reattaching reconnects a client to an existing session, restoring the view.
- **Multiple clients can attach to the same session** (shared view: all clients see the same active window/pane).

#### Canonical grid size & the viewport model

This is a key design decision that differs from tmux's behavior and avoids its most common annoyance:

- Each pane has a **canonical grid size** (rows × cols). This is set by the **first client to attach** (or create the session) from its terminal size.
- **lrmux never auto-resizes the canonical grid when a client connects or disconnects.** Attaching from a smaller or larger terminal does not change the pane size and does not send `SIGWINCH` to the child process.
- Instead, each client renders a **viewport** into the canonical grid:
  - **Client smaller than canonical grid**: the client shows the top-left portion of the grid that fits; the rest is not visible (the user can scroll/pan if we add that later). No content is lost — the full grid is still on the server.
  - **Client larger than canonical grid**: the grid is rendered at its canonical size, and the remaining area is filled with a **filler region** (a dim background with a thin border line separating it from the content — see §2.12).
- **Manual resize** (`Prefix F`): the user explicitly triggers a resize to match the current client's terminal size. This:
  1. Updates the canonical grid size to the current client's size.
  2. Sends `SIGWINCH` to the child process so it redraws at the new size.
  3. All other attached clients adjust their viewports to the new canonical size (they may now show filler or a cropped view depending on their own terminal size).

This means: connecting from a phone or small terminal won't shrink everyone's panes. Only an explicit `Prefix F` resizes, and the user who triggered it is in control.

### 2.6 Scrollback & copy mode

- Each pane maintains a scrollback buffer (configurable size, default 10,000 lines).
- A copy/scrollback mode (entered via `Prefix [` or `F7`) lets the user navigate history, search, and select text to copy.
- **vi-style keybindings only in v1** (emacs-style deferred to a later phase).
- **Clipboard integration**: copied text goes to both an internal paste buffer (paste with `Prefix ]`) and the **system clipboard**. lrmux auto-detects the clipboard tool: `pbcopy` (macOS), `xclip`/`xsel` (X11), `wl-copy` (Wayland). Overridable via config (`behavior.clipboard_cmd`).

### 2.7 Status bar

A byobu-style status line (bottom of the screen by default) showing:

- Server name
- Current session name
- Window list (with active window highlighted)
- Active pane indicator
- Hostname
- Clock

All elements are configurable and the status bar can be disabled entirely.

### 2.8 Truecolor support

- lrmux sets `TERM` to a truecolor-capable value and `COLORTERM=truecolor` for child processes so they emit 24-bit color escape sequences.
- The grid stores full 24-bit foreground/background color per cell.
- Rendering emits truecolor SGR sequences (`\e[38;2;R;G;Bm` / `\e[48;2;R;G;Bm`).
- A fallback mode for non-truecolor terminals is documented but not a v1 priority (modern terminals overwhelmingly support truecolor).

### 2.9 Configurable prefix key

- Default prefix: `Ctrl-A` (byobu/screen convention).
- Configurable via config file or `lrmux set prefix <key>`.
- Documented conflict: `Ctrl-A` is readline/emacs "move to beginning of line"; the user explicitly wants this default, and it's trivially changeable.

### 2.10 Mouse support

- **lrmux does not parse or handle mouse events.** All mouse input (scroll wheel, clicks, drags) is passed through to the child process untouched, exactly like every other non-prefix key.
- This keeps the passthrough invariant pure: only the prefix key (and optionally F-keys) is ever intercepted; everything else — keyboard and mouse alike — flows to the child. AI CLIs, TUIs, and terminal apps that use the mouse get full, unmodified mouse control.
- No mouse-related config option is needed in v1 (there's nothing to toggle). If mouse-aware features like click-to-select-pane or drag-to-resize are ever added, they would be a separate opt-in layer in a future phase.

### 2.11 Window numbering

- Windows are numbered starting at **0** (tmux convention).
- **Auto-renumber on close**: when a window is closed, remaining windows are renumbered to fill gaps (so 0,1,2 stays contiguous after closing window 1). This combines tmux's start-at-0 with byobu's contiguous numbering.
- Auto-renumber can be disabled in config (`behavior.renumber_windows = false`).

### 2.12 Default shell

- New panes spawn the user's `$SHELL`, falling back to `/bin/sh` if `$SHELL` is unset.
- Overridable per-session or globally in config (`behavior.default_shell`).

### 2.13 Session & server naming

- The user may specify a name explicitly (`lrmux new -s myname`).
- If no name is given, lrmux derives one from the **current working directory**:
  - Use the directory's basename (e.g. `lrmux` if launched in `~/dev/lrmux`).
  - If the directory is the user's home, use the name `home`.
  - If a session with that name already exists, append a suffix: `name-2`, `name-3`, etc.
- Servers follow the same rule when created without an explicit name (default server is named `default`).

### 2.14 Child process exit behavior

- **Configurable**. Default behavior:
  - **Exit code 0** (success): the pane auto-closes. If it was the last pane in the window, the window closes too (and auto-renumber applies).
  - **Exit code ≠ 0** (failure): the pane stays open, showing a `[process exited, code N]` message so the user can read the error output and scroll the last output before closing it manually (`Prefix x`).
- Configurable via `behavior.remain_on_exit` (always keep) or `behavior.close_on_exit` (always close) to override the default code-based logic.

### 2.15 Server lifecycle

- The server **persists** when the last client detaches — sessions and child processes keep running indefinitely. This is the default.
- The server exits only when:
  - All sessions are explicitly closed (no sessions remain), or
  - The user explicitly kills it (`lrmux kill-server`).
- This differs from tmux's `exit-empty` default; lrmux defaults to persistence so background work is never accidentally lost. (A future `exit_unattached` timeout option may be added for users who want auto-cleanup.)

### 2.16 Filler region (for larger clients)

- When a client terminal is larger than the canonical grid, the area beyond the grid is the **filler region**.
- Style: a **dim background color** (configurable, e.g. dark gray) with a **thin border line** separating the content viewport from the filler. This clearly signals "no content here" without looking like a rendering bug.
- Filler color and border are configurable in `[colors]`.

### 2.17 Startup selector UX

Running `lrmux` with no arguments:

- **If exactly one server and one session exist**: auto-join it immediately (no selector shown). Fast path for the common case.
- **Otherwise**: show the interactive selector.

The interactive selector:

- A **flat list** of `server / session` entries across all running servers (e.g. `default / lrmux`, `default / home`, `work / api`).
- **Fuzzy filter**: typing filters the list by fuzzy match on the `server / session` string (fzf-style).
- `j`/`k` or arrow keys to navigate, `Enter` to join the selected session.
- `n` creates a new session: prompts for a name with a **default derived from CWD** (per §2.13) pre-filled; user can accept or edit it. Session is created on the currently highlighted server (or `default` if none highlighted).
- `N` creates a new server + session: prompts for a server name (default: `default` or `default-2` etc. on collision) and a session name (default: CWD-derived), then spawns both.
- If no servers are running, the selector shows a single "create new server + session" prompt with defaults pre-filled.

### 2.18 In-session session manager

From within an attached session, `Prefix M` opens the **session manager** — the same selector UI from §2.17, but overlaid on the current session. This lets the user switch sessions or servers without detaching first.

- `Prefix M` opens the selector; selecting a session switches to it (the current session stays alive on the server).
- `n` / `N` work as in the startup selector (new session / new server + session).
- `Esc` or `q` closes the selector and returns to the current session.

### 2.19 Configuration file

- Location: `~/.config/lrmux/config.toml` (TOML format).
- Sections: `[prefix]`, `[keys]`, `[fkeys]`, `[statusbar]`, `[colors]`, `[behavior]`.
- All keybindings remappable; F-key shortcuts toggleable.
- See §7 for format details and examples.

### 2.20 AI-CLI friendliness (base scope)

The **base** requirement is the passthrough invariant from §1: only the prefix (and optionally F-keys) is intercepted. Mouse events are always passed through. This alone makes lrmux safe for AI CLIs.

Future AI-specific enhancements (programmatic API, pane metadata, AI-aware layouts) are **out of base scope** and tracked in the roadmap (§8, Phase 7).

---

## 3. Non-Functional Requirements

- **Performance**: Rust; minimal allocations on the hot path (PTY read → relay → render); diff-based screen rendering (only emit changed cells/regions).
- **Minimal dependencies**: `nix` (PTY/termios/event loop) + `libc` for the system layer. A small set of well-vetted crates for ancillary needs (TOML parsing, VT parsing) is acceptable but kept lean. No heavy frameworks.
- **Portability**: macOS + Linux in v1 (both support Unix domain sockets, `forkpty`, `termios`, `poll`/`kqueue`/`epoll`). Windows is explicitly out of scope for v1.
- **Single binary**: one `lrmux` executable; no separate server package.
- **Robustness**:
  - Server survives client crashes (client sockets are cleaned up; sessions persist).
  - Child processes are cleanly terminated on session kill (SIGHUP → SIGKILL escalation).
  - Server persists when the last client detaches; exits only when all sessions are closed or explicitly killed (see §2.15).
- **Security**: Unix socket permissions set to `0600` (owner-only); socket path under `/tmp/lrmux-UID/` (per-user, like tmux). No network exposure in v1.

---

## 4. Architecture

### 4.1 Process model

```
┌─────────────────┐       Unix socket        ┌──────────────────┐
│  lrmux client   │ ◄──────────────────────► │  lrmux server     │
│  (your terminal)│                          │  (daemon)         │
└─────────────────┘                          │                   │
                                             │  ┌─────────────┐  │
                                             │  │ Session 1   │  │
                                             │  │  Window 1   │  │
                                             │  │   Pane (PTY)│  │
                                             │  │   Pane (PTY)│  │
                                             │  │  Window 2   │  │
                                             │  │   Pane (PTY)│  │
                                             │  └─────────────┘  │
                                             │  ┌─────────────┐  │
                                             │  │ Session 2   │  │
                                             │  └─────────────┘  │
                                             └──────────────────┘
```

- The **server** owns all session/window/pane state, manages PTY children, and runs the event loop.
- A **client** connects to the server, relays local terminal input to the server, and renders server output to the local terminal.
- The same `lrmux` binary serves both roles; the client forks the server on first use.

### 4.2 IPC transport

- **v1**: Unix domain sockets at `/tmp/lrmux-UID/<server-name>` (default server name: `default`).
- The transport is abstracted behind a trait so **TCP can be added later** without rewriting the protocol layer (see §8, Phase 7).
- Socket permissions: `0600` (owner-only).

### 4.3 Wire protocol

Length-prefixed binary messages between client and server. Each message: a 4-byte length header (u32, little-endian) + a 1-byte message type + payload.

**Message types** (initial set, to be refined during Phase 3):

| Direction | Type | Purpose |
|-----------|------|---------|
| C → S | `Identify` | Client sends terminal info (size, `$TERM`, capabilities) |
| S → C | `IdentifyAck` | Server acknowledges, sends session list |
| C → S | `ListServers` | Request list of known servers (for selector) |
| S → C | `ServerList` | List of server names + socket paths |
| C → S | `ListSessions` | Request sessions on a server |
| S → C | `SessionList` | Sessions with metadata |
| C → S | `AttachSession` | Attach to a session by name/id |
| C → S | `NewSession` | Create + attach a new session |
| C → S | `Detach` | Detach from current session |
| C → S | `PaneInput` | Keystrokes from client → active pane |
| S → C | `PaneOutput` | Rendered output / grid diffs → client |
| C → S | `Resize` | Client terminal resized → server adjusts panes |
| C → S | `WindowCommand` | Create/switch/kill/rename window |
| C → S | `PaneCommand` | Split/kill/resize/focus pane |
| S → C | `StatusBarUpdate` | Status bar content |
| S → C | `Error` | Error message |

### 4.4 Event loop

- **Single-threaded** event loop in the server using `poll(2)` (portable) or `kqueue` (macOS/BSD) / `epoll` (Linux) via `nix::sys::event`.
- Non-blocking I/O on all PTY master fds and client sockets.
- The loop processes: PTY output → VT parse → grid update → schedule render; client input → prefix detection → command dispatch or relay to pane; control messages → state changes.
- **No async runtime** (no tokio/async-std) — a manual event loop keeps dependencies minimal and matches the "minimal deps" preference. (Deliberate choice; revisit if maintenance becomes a burden.)

### 4.5 Data model

Mirrors tmux's hierarchy, simplified:

```rust
struct Server {
    sessions: HashMap<SessionId, Session>,
    clients: HashMap<ClientId, Client>,
    options: ServerOptions,
    socket_path: PathBuf,
}

struct Session {
    id: SessionId,
    name: String,
    windows: Vec<Window>,
    active_window: usize,
    attached_clients: HashSet<ClientId>,
    options: SessionOptions,
    created_at: Instant,
}

struct Window {
    id: WindowId,
    name: String,
    panes: Vec<Pane>,
    active_pane: usize,
    layout: Layout,
}

struct Pane {
    id: PaneId,
    pty_master: OwnedFd,
    child_pid: Pid,
    grid: Grid,           // visible content + scrollback
    size: PaneSize,       // rows, cols
    mode: PaneMode,       // Normal | Copy | ...
}

struct Grid {
    rows: Vec<Row>,
    scrollback: RingBuffer<Row>,  // history above the visible region
    cursor: Cursor,
}

struct Row {
    cells: Vec<Cell>,
}

struct Cell {
    ch: char,
    fg: Color,   // 24-bit
    bg: Color,   // 24-bit
    attrs: Attr, // bold, italic, underline, etc.
}

enum Color {
    Default,
    Indexed(u8),      // 256-color palette
    TrueColor(u8, u8, u8),
}
```

### 4.6 Rendering

- The server maintains a logical grid per pane (the canonical grid).
- On PTY output, the VT parser updates the grid.
- **Rendering model (decided)**: the **server sends the canonical grid** to each client; each **client computes and renders its own viewport** locally (cropping to fit if smaller, drawing filler if larger — see §2.5/§2.16). This keeps the server simpler (no per-client rendering) and lets each client handle its own size without round-trips.
- Client-side rendering is **diff-based**: the client compares its last-rendered viewport to the current one and emits only the escape sequences needed to update changed cells (cursor move + SGR + char).
- The status bar is rendered as a separate row (server pushes status bar content; client renders it).

---

## 5. Tech Stack Details

### 5.1 Language

- **Rust**, edition 2021.

### 5.2 System layer

- **`nix`** crate, features: `term`, `process`, `event`.
  - `nix::pty::forkpty` / `openpty` — PTY creation.
  - `nix::sys::termios` — raw mode on the client's controlling terminal.
  - `nix::sys::event` — `kqueue` (macOS) / `epoll` (Linux) event loop; `poll(2)` as a portable fallback.
- **`libc`** — as needed for constants/syscalls not covered by `nix`.

### 5.3 Terminal I/O (client side)

- Raw termios mode on the client's controlling terminal (enter on startup, restore on exit).
- **Hand-rolled escape-sequence writer** (no `crossterm`/`termion`): cursor positioning, clear, SGR with truecolor (`38;2;r;g;b` / `48;2;r;g;b`), alternate screen enter/exit, scroll regions.
- Rationale: minimal deps, full control, truecolor as a first-class concern rather than an add-on.

### 5.4 VT parsing (child output)

- **`vte`** crate — tiny, well-maintained, zero-dep VT parser. This is the **one justified ancillary dependency** beyond `nix`/`libc`.
- Rationale: hand-rolling a correct VT parser is a significant source of bugs; `vte` is ~pure parsing with no I/O opinions, ~small surface, and widely vetted. The tradeoff against "minimal deps" is worth it here.
- Alternative (noted): hand-roll a minimal parser covering only the sequences we care about; revisit if `vte` becomes a maintenance liability.

### 5.5 Configuration

- **`toml`** crate (with `serde`) for parsing `~/.config/lrmux/config.toml`.
- Minimal; TOML is human-friendly and matches the config's structure.

### 5.6 Async runtime

- **None.** A manual `poll`/`kqueue`/`epoll` event loop via `nix`. This is a deliberate choice to keep dependencies minimal. (Tradeoff: more manual code vs. fewer deps and full control.)

### 5.7 Proposed module layout

```
src/
├── main.rs          # entry, arg parsing, client/server fork decision
├── server/          # server process: event loop, state, session/window/pane mgmt
│   ├── mod.rs
│   ├── event_loop.rs
│   ├── session.rs
│   ├── window.rs
│   └── pane.rs
├── client/          # client process: raw mode, input relay, output render
│   ├── mod.rs
│   ├── terminal.rs
│   └── render.rs
├── pty/             # PTY spawn/resize/read/write over nix
│   └── mod.rs
├── term/            # termios raw mode, escape-sequence writer, truecolor SGR
│   ├── mod.rs
│   └── escapes.rs
├── vt/              # VT parser (vte wrapper) → grid updates
│   └── mod.rs
├── grid/            # screen grid + scrollback ring buffer, cell/attr types
│   ├── mod.rs
│   ├── cell.rs
│   └── scrollback.rs
├── proto/           # client↔server message protocol (encode/decode)
│   └── mod.rs
├── ipc/             # Unix socket transport (abstracted for future TCP)
│   └── mod.rs
├── config/          # TOML config loading + keybinding map
│   └── mod.rs
├── keys/            # key input parsing, prefix detection, command dispatch
│   └── mod.rs
├── statusbar/       # status line rendering
│   └── mod.rs
└── layout/          # pane split/resize layout algorithms
    └── mod.rs
```

---

## 6. Keybinding Specification

### 6.1 Prefix key

- **Default**: `Ctrl-A` (byobu/screen convention).
- Pressing the prefix enters **command mode** for the next keystroke.
- If the next keystroke is not a bound command, it is discarded (does not fall through to the pane). This avoids accidental input.
- Configurable via `~/.config/lrmux/config.toml` or `lrmux set prefix <key>`.
- To send a literal `Ctrl-A` to the child, press the prefix key **twice** (double-prefix). This is the tmux convention and works regardless of what the prefix is set to.

### 6.2 Prefix commands

#### First iteration (minimal usable set)

These are the keybindings for the first working version — enough to use lrmux as a tabbed terminal with scrollback:

| Key (after prefix) | Action |
|--------------------|--------|
| `c` | Create new window |
| `n` | Next window |
| `p` | Previous window |
| `0`–`9` | Select window by index |
| `[` | Enter scrollback/copy mode |
| `?` | Show keybindings (help) |
| `d` | Detach from session |
| `x` | Kill active pane (with confirmation) |
| `M` | Open session manager (switch/create session or server without detaching) |
| Double prefix | Send literal prefix key to child |

#### Later iterations (full set)

Added incrementally as features are built:

| Key (after prefix) | Action | Phase |
|--------------------|--------|-------|
| `%` | Vertical split (split left/right) | Phase 4 (splits) |
| `"` | Horizontal split (split top/bottom) | Phase 4 (splits) |
| `o` | Cycle pane focus (next) | Phase 4 (splits) |
| `;` | Cycle pane focus (alias for `o`) | Phase 4 (splits) |
| `z` | Toggle pane zoom (maximize/restore) | Phase 4 (splits) |
| `Space` | Cycle pane layout | Phase 4 (splits) |
| `F` | Resize canonical grid to current client's terminal size | Phase 4 (viewport) |
| `]` | Paste from internal paste buffer | Phase 5 (copy mode) |
| `,` | Rename current window | Phase 4+ |
| `$` | Rename current session | Phase 6 |
| `&` | Kill active window (with confirmation) | Phase 4+ |
| `s` | List sessions (switch) | Phase 6 |
| `S` | List servers (switch server) | Phase 6 |
| `r` | Reload config | Phase 5 (config) |
| `:` | Command prompt | Phase 5+ |
| `q` | Briefly show pane numbers | Phase 4+ |

### 6.3 F-key shortcuts (byobu-style, work without prefix)

These are enabled by default for byobu ergonomics but **can be disabled entirely** in config so they pass through to the child (important for AI CLIs that may use F-keys).

| Key | Action |
|-----|--------|
| `F2` | New window |
| `F3` | Previous window |
| `F4` | Next window |
| `F6` | Detach |
| `F7` | Scrollback/copy mode |
| `F8` | Rename window |
| `F9` | Configuration menu |
| `F12` | Lock terminal |
| `Shift-F2` | Horizontal split |
| `Ctrl-F2` | Vertical split |
| `Shift-F3` | Focus previous pane |
| `Shift-F4` | Focus next pane |
| `Shift-F5` | Join all splits (unsplit) |
| `Ctrl-F6` | Remove active pane |

### 6.4 Scrollback / copy mode keys

vi-style by default (configurable to emacs-style in future):

| Key | Action |
|-----|--------|
| `h` `j` `k` `l` | Move cursor left/down/up/right |
| `0` `^` `$` | Start/end of line |
| `g` `G` | Top / bottom of scrollback |
| `Ctrl-u` `Ctrl-d` | Half-page up / down |
| `Ctrl-b` `Ctrl-f` | Full page up / down |
| `/` | Search forward |
| `?` | Search backward |
| `n` `N` | Next / previous search match |
| `Space` | Begin/end selection |
| `Enter` | Copy selection to paste buffer |
| `q` | Quit copy mode |
| `Esc` | Quit copy mode |

### 6.5 Configurability

- All prefix commands and F-key shortcuts are remappable in `~/.config/lrmux/config.toml`.
- F-key shortcuts can be disabled entirely (`[fkeys] enabled = false`) so they pass through to the child.
- The passthrough guarantee is a **documented invariant**: only the prefix (and optionally F-keys) is ever intercepted; no other input — keyboard or mouse — is consumed by lrmux.

---

## 7. Configuration Format

### 7.1 Location

`~/.config/lrmux/config.toml` (TOML).

### 7.2 Sections

```toml
# Prefix key
[prefix]
key = "ctrl-a"          # default; e.g. "ctrl-b", "ctrl-space"
double_send = true       # double-prefix sends the prefix key literally to the child

# Prefix-command bindings (first iteration; more added in later phases)
[keys]
"c" = "new-window"
"n" = "next-window"
"p" = "previous-window"
"0" = "select-window-0"
"1" = "select-window-1"
"2" = "select-window-2"
"3" = "select-window-3"
"4" = "select-window-4"
"5" = "select-window-5"
"6" = "select-window-6"
"7" = "select-window-7"
"8" = "select-window-8"
"9" = "select-window-9"
"[" = "copy-mode"
"?" = "show-keys"
"d" = "detach"
"x" = "kill-pane"
"M" = "session-manager"
# Later iterations add: %, ", o, ;, z, Space, F, ], ,, $, &, s, S, r, :, q

# F-key shortcuts (disable entirely for full passthrough)
[fkeys]
enabled = true
"F2" = "new-window"
"F3" = "previous-window"
"F4" = "next-window"
"F6" = "detach"
"F7" = "copy-mode"
"F8" = "rename-window"
"F9" = "config-menu"
"Shift-F2" = "split-horizontal"
"Ctrl-F2" = "split-vertical"
"Shift-F3" = "prev-pane"
"Shift-F4" = "next-pane"
"Ctrl-F6" = "kill-pane"

# Status bar
[statusbar]
enabled = true
position = "bottom"      # "bottom" | "top"
elements = ["server", "session", "windows", "hostname", "clock"]

# Colors (truecolor)
[colors]
statusbar_fg = "255,255,255"
statusbar_bg = "30,30,46"
active_window_fg = "137,180,250"
active_window_bg = "30,30,46"
filler_bg = "40,40,55"          # filler region background (larger clients)
filler_border = "60,60,75"      # filler border line color

# Behavior
[behavior]
default_shell = ""           # empty = use $SHELL, fallback /bin/sh
scrollback_lines = 10000
confirm_kill = true
renumber_windows = true      # auto-renumber windows on close (start at 0)
# Child exit: default = auto-close on code 0, keep on non-zero.
# Override with one of:
#   remain_on_exit = true   # always keep pane after child exits
#   close_on_exit = true    # always close pane after child exits
clipboard_cmd = ""          # empty = auto-detect (pbcopy/xclip/wl-copy)
```

---

## 8. Development Roadmap

### Phase 0 — Bootstrap (foundation)

- `cargo init`, workspace setup.
- CI: `cargo fmt --check`, `cargo clippy`, `cargo test`.
- `AGENTS.md` with build/test commands and project conventions.
- Module skeleton with stubs (per §5.7).

**Deliverable**: a compiling, empty project with CI and module structure.

### Phase 1 — PTY + single shell (no multiplexing)

- `src/pty/`: `forkpty`-based spawn, resize, read/write.
- `src/term/`: raw mode enter/exit, basic escape writer.
- Minimal `lrmux` that runs one shell in a PTY and relays I/O to the local terminal.

**Deliverable**: `lrmux` runs a shell in a PTY with raw terminal I/O. Proves the PTY + raw-mode loop.

### Phase 2 — Grid + VT parser + rendering

- `src/grid/` + `src/vt/`: parse PTY output into a logical grid with truecolor attrs; render grid to terminal with diff-based updates.
- Scrollback ring buffer.

**Deliverable**: panes render correctly with truecolor; scrollback works. Foundation for splits and copy mode.

### Phase 3 — Server/client split + IPC

- `src/proto/` + `src/ipc/`: message protocol, Unix socket transport.
- `src/server/` + `src/client/`: fork server, client connects, relay I/O over the socket.
- Single session, single window, single pane over the client/server boundary.

**Deliverable**: the shell runs on the server; the client connects over a Unix socket and renders. Proves the architecture.

### Phase 4 — Sessions, windows, panes, viewport

- Full `Server → Session → Window → Pane` data model.
- Multiple windows (numbered from 0, auto-renumber on close), window switching, pane splits (horizontal/vertical), layout engine, pane focus cycling.
- **Viewport model** (§2.5): canonical grid size set by first client; clients render viewports (crop/filler); `Prefix F` manual resize with `SIGWINCH`.
- **Filler region** rendering (§2.16) for larger clients.
- Status bar.
- **Child exit behavior** (§2.14): auto-close on code 0, keep on non-zero.

**Deliverable**: a usable multiplexer with splits, multiple windows, viewport rendering, and a status bar.

### Phase 5 — Keybindings, prefix, copy mode, config

- `src/keys/` + `src/config/`: prefix detection, command mode.
- **First-iteration keybindings** (§6.2): `c` (new window), `n`/`p` (next/prev window), `0`–`9` (select window), `[` (copy mode), `?` (help), `d` (detach), `x` (kill pane), double-prefix (send literal prefix).
- F-key shortcuts (toggleable).
- **Scrollback/copy mode** (§2.6): vi-style keys, search, selection.
- **Clipboard integration** (§2.6): internal paste buffer + system clipboard (auto-detect `pbcopy`/`xclip`/`wl-copy`).
- TOML config loading (all sections from §7).
- Later-iteration keybindings (splits, pane focus, rename, etc.) added as their features land in Phase 4+.

**Deliverable**: full byobu-style keybinding + copy/clipboard experience; configurable.

### Phase 6 — Multi-server + startup selector

- Server naming (§2.13), multiple concurrent servers, socket-per-server.
- **Interactive startup selector** (§2.17): flat `server / session` list with fuzzy filter; `j`/`k` navigation, `Enter` to join, `n` for new session, `N` for new server.
- Session naming from CWD (§2.13): directory basename, `home` for home dir, `name-2` suffix on collision.
- CLI: `lrmux` (no args) → selector; `lrmux attach -s <session>`; `lrmux new -s <name>`; `lrmux ls-servers`; `lrmux ls-sessions`; `lrmux kill-server`.

**Deliverable**: the day-1 user experience — run `lrmux`, pick a server and session, get to work.

### Phase 7 — Polish & AI-CLI niceties (future, out of base scope)

- **Programmatic API** for AI CLIs: attach, send keys, read pane output, create sessions — over the IPC socket or a separate control socket.
- **Pane metadata**: detect AI CLI processes running in panes, surface status (model, token usage) in the status bar.
- **AI-aware split layouts**: auto-arrange an AI CLI alongside an editor or output pane.
- **TCP transport**: remote attach over the network (the IPC abstraction makes this additive).
- **Config menu (F9)**: interactive configuration UI.
- **Independent per-client views**: multiple clients on one session with independent window selection.

**Deliverable**: the differentiators that go beyond byobu/tmux parity.

---

## 9. Risks & Considerations

### 9.1 VT parsing correctness

Terminal emulation is fiddly (CSI sequences, OSC, DCS, charset switching, etc.). Hand-rolling a correct parser is a significant source of bugs. **Recommendation**: use the `vte` crate as the one justified exception to "minimal deps." It's tiny, well-vetted, and pure parsing. Tradeoff documented; revisit if it becomes a liability.

### 9.2 Event loop complexity

A hand-rolled `poll`/`kqueue` loop is more code but fewer deps. `mio` would reduce code but add a dependency. **Recommendation**: hand-rolled for v1 (matches the minimal-deps preference); revisit if maintenance becomes a burden.

### 9.3 macOS vs Linux PTY quirks

`forkpty` exists on both via `nix`, but `ptsname`/`grantpt` paths differ slightly. **Mitigation**: abstract all PTY operations behind `src/pty/` and test on both platforms in CI.

### 9.4 Truecolor detection

lrmux sets `TERM` and `COLORTERM=truecolor` so children emit 24-bit sequences. **Consideration**: some older terminals don't support truecolor; document a config fallback (`colors.mode = "256" | "truecolor"`) for v1.1. Not a v1 blocker — modern terminals overwhelmingly support truecolor.

### 9.5 Prefix-key conflict (`Ctrl-A` vs readline/emacs)

`Ctrl-A` is readline/emacs "move to beginning of line." The user explicitly wants this default (byobu convention), and it's trivially changeable in config. The conflict is documented in §2.9 and the config. `double_send = true` (press prefix twice for a literal `Ctrl-A`) mitigates it.

### 9.6 Multi-client views & the viewport model (decided)

When multiple clients attach to one session, v1 uses **shared view** (all clients see the same active window/pane) with the **viewport model** from §2.5: the canonical grid has a fixed size (set by the first client, changeable only via explicit `Prefix F`), and each client renders a viewport into it (cropping or filling as needed). This avoids tmux's annoyance of shrinking everyone's panes when a small client attaches. Independent per-client window selection is deferred to Phase 7.

### 9.7 Rendering model (decided)

**Server sends the canonical grid; each client computes and renders its own viewport** (diff-based). This was chosen over server-side rendering because the viewport model requires each client to handle its own size anyway — pushing viewport logic to the client keeps the server simpler and avoids per-client rendering state on the server. Tradeoff: slightly smarter client code, but the client already needs viewport/filler logic. Revisit if profiling shows the grid-transfer bandwidth is a problem (unlikely for local Unix sockets).

---

*This document is the design source of truth for lrmux. It will be updated as implementation progresses and design decisions are confirmed during each phase.*
