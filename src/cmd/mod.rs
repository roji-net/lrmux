// cmd: tmux-style command parser shared by CLI and control mode.
//
// Parses commands like:
//   new-session -s <name> -c <cwd> [-- <cmd>]
//   send-keys -t <session>:<window> <keys>
//   capture-pane -t <target> -p
//
// The parser is shared between the CLI (main.rs) and the control mode
// client, so both use identical syntax.

use std::collections::HashMap;

/// A parsed target: [session]:[window] (tmux-style).
/// Session and window are optional — empty means "current".
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub session: Option<String>,
    pub window: Option<String>,
}

impl Target {
    /// Parse a target string like "session", "session:0", ":0", "0", or "".
    /// A bare number is treated as a window index (tmux convention).
    pub fn parse(s: &str) -> Self {
        if s.is_empty() {
            return Self::default();
        }
        if let Some((sess, win)) = s.split_once(':') {
            Self {
                session: if sess.is_empty() {
                    None
                } else {
                    Some(sess.to_string())
                },
                window: if win.is_empty() {
                    None
                } else {
                    Some(win.to_string())
                },
            }
        } else if s.chars().all(|c| c.is_ascii_digit()) {
            // Bare number → window index in current session.
            Self {
                session: None,
                window: Some(s.to_string()),
            }
        } else {
            Self {
                session: Some(s.to_string()),
                window: None,
            }
        }
    }
}

/// Parsed flags from a command line.
/// Stores both short (-s value) and long (--session value) flags,
/// plus positional args and everything after `--`.
#[derive(Default)]
pub struct ParsedCmd {
    /// Short flags: "-s" → "value", "-d" → "" (boolean flags have empty value).
    pub flags: HashMap<String, String>,
    /// Positional arguments (not flags, not after `--`).
    pub positional: Vec<String>,
    /// Everything after `--` (the command to run).
    pub after_dash: Vec<String>,
}

impl ParsedCmd {
    /// Get a flag value by any of its names (e.g. "s" or "session").
    pub fn get(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(|s| s.as_str())
    }

    /// Check if a boolean flag is set.
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    /// Get the target (-t / --target).
    pub fn target(&self) -> Target {
        match self.get("t").or_else(|| self.get("target")) {
            Some(t) => Target::parse(t),
            None => Target::default(),
        }
    }
}

/// Parse a command line into a ParsedCmd.
/// Supports:
///   -s value, -s=value       (short flag with value)
///   --session value, --session=value  (long flag with value)
///   -d, -p, -q               (boolean flags, no value)
///   positional args          (non-flag args before `--`)
///   -- args                  (everything after `--` is captured)
pub fn parse_flags(args: &[String]) -> ParsedCmd {
    let mut cmd = ParsedCmd::default();
    let mut i = 0;
    let mut after_dash = false;

    while i < args.len() {
        if after_dash {
            cmd.after_dash.push(args[i].clone());
            i += 1;
            continue;
        }

        let arg = &args[i];

        if arg == "--" {
            after_dash = true;
            i += 1;
            continue;
        }

        // Long flag: --name or --name=value
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some((name, value)) = rest.split_once('=') {
                cmd.flags.insert(name.to_string(), value.to_string());
            } else {
                // Could be a boolean flag or a flag with value in next arg.
                // Known boolean flags: d (detach), p (print), q (quiet), P (detached)
                if matches!(
                    rest,
                    "detach" | "print" | "quiet" | "detached" | "colors" | "clipboard" | "headless"
                ) {
                    cmd.flags.insert(rest.to_string(), String::new());
                } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    cmd.flags.insert(rest.to_string(), args[i + 1].clone());
                    i += 1;
                } else {
                    cmd.flags.insert(rest.to_string(), String::new());
                }
            }
            i += 1;
            continue;
        }

        // Short flag: -s value, -s=value, -d (boolean)
        if arg.starts_with('-') && arg.len() >= 2 && arg != "-" {
            let flag_part = &arg[1..];
            if let Some((name, value)) = flag_part.split_once('=') {
                cmd.flags.insert(name.to_string(), value.to_string());
            } else {
                // Known boolean short flags: d, p, q, P
                if matches!(flag_part, "d" | "p" | "q" | "P") {
                    cmd.flags.insert(flag_part.to_string(), String::new());
                } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    cmd.flags.insert(flag_part.to_string(), args[i + 1].clone());
                    i += 1;
                } else {
                    cmd.flags.insert(flag_part.to_string(), String::new());
                }
            }
            i += 1;
            continue;
        }

        // Positional argument.
        cmd.positional.push(arg.clone());
        i += 1;
    }

    cmd
}

/// One CLI command: canonical name, aliases, and its own help text.
pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub summary: &'static str,
    /// Usage lines, already indented for printing under `USAGE:`.
    pub usage: &'static str,
    pub body: &'static str,
}

/// Commands shown by `lrmux --help`. Aliases share this entry's help.
pub static COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "attach",
        aliases: &["attach-session"],
        summary: "Attach to a session",
        usage: "    \
lrmux attach [-s <server>] [-t <[server:]session> | <target>]\n    \
lrmux attach <host:port> [<session>]\n    \
lrmux attach --tcp <host:port> [-t <session>]\n    \
lrmux attach -CC [-s <server>]",
        body: "\
-s, --server <name|host:port>   Local server name, or a TCP address\n\
-t, --target <session>          Session to attach\n\
--tcp <host:port>               Connect over TCP instead of a local socket\n\
-CC                             iTerm2 control mode\n\
\n\
A target shaped like <ipv4>:<port>, [<ipv6>]:<port>, or <host.domain>:<port>\n\
opens a TCP connection. It is not looked up as a Unix socket.\n\
<server>:<session> selects a session on a local server when <server> is not\n\
an address (or when a local server with that name is already running).\n\
\n\
The same PSK must be set on both sides (config network.psk, LRMUX_PSK, or --psk).\n\
Telnet and nc will connect and drop: the TCP port speaks TLS, not a text protocol.",
    },
    CommandSpec {
        name: "new-session",
        aliases: &["new"],
        summary: "Create a session and attach",
        usage: "    lrmux new-session [-s <name>] [-c <cwd>] [-d|--headless] [--] [<cmd> [args...]]",
        body: "\
-s, --session <name>       Session name\n\
-c, --cwd <dir>            Start directory (not a shell command)\n\
-d, --detach, --headless   Create the session and return to the shell.\n    \
If no server is running, start a headless one first.\n\
-- <cmd> [args...]         Command for the first window ($SHELL -ci)",
    },
    CommandSpec {
        name: "new-server",
        aliases: &["start-server"],
        summary: "Start a server and attach",
        usage: "    \
lrmux new-server [-s <name>] [--tcp <addr>] [--ws <addr>] [--headless] [-CC] [--] [<cmd> [args...]]",
        body: "\
-s, --server <name>    Server name (default: an unused local name).\n    \
The first session gets this name too.\n\
--tcp <addr>           Listen address, e.g. 0.0.0.0:17281.\n    \
A trailing + (0.0.0.0:17280+) binds the first free port from there.\n    \
This is where the server binds. The local client still\n    \
attaches on the Unix socket, not via this address.\n    \
Config: tcp_listen = \"auto\" in ~/.config/lrmux/config.toml does the same\n    \
for every server, without passing --tcp.\n\
--ws <addr>            WebSocket listen address\n\
--headless             Start the server and return. Do not attach.\n    \
`start-server` is an alias of `new-server --headless`.\n\
-CC                    iTerm2 control mode (the server does not take the terminal)\n\
-- <cmd> [args...]     Command for the first session\n\
\n\
Without --headless the server stays in the foreground client. Headless is opt-in.",
    },
    CommandSpec {
        name: "manager",
        aliases: &[],
        summary: "Start a standalone session manager (peer directory)",
        usage: "    lrmux manager [-s <name>] [--tcp <addr>]",
        body: "\
-s, --server <name>    Manager name (default: \"manager\").\n    \
The name is its Unix socket: /tmp/lrmux-<UID>/<name>\n\
--tcp <addr>           Listen address for registrations and peer queries.\n    \
Default: network.tcp_listen from config, else \"auto\"\n    \
(binds the first free port from 17280).\n\
\n\
A manager hosts no sessions. It holds the peer cache, answers\n    \
ListPeers, accepts Register announcements from other nodes, and stays\n    \
running until killed. Intended for always-on rendezvous nodes\n    \
(e.g. a home server or add-on). See docs/MESH.md.\n\
\n\
Peer nodes register by listing the manager under [peers] managers\n    \
in their config.toml. A normal server can also act as a directory\n    \
with [peers] directory = true.",
    },
    CommandSpec {
        name: "new-window",
        aliases: &["neww"],
        summary: "Create a window",
        usage: "    lrmux new-window [-t <target>] [-n <name>] [-c <cwd>] [--] [<cmd> [args...]]",
        body: "\
-t, --target <target>   Session (and optional window) to act on\n\
-n, --name <name>       Window name\n\
-c, --cwd <dir>         Start directory\n\
-- <cmd> [args...]      Command for the new window",
    },
    CommandSpec {
        name: "select-session",
        aliases: &[],
        summary: "Switch the attached client to a session",
        usage: "    lrmux select-session -t <name>",
        body: "-t, --target <name>   Session name on the current server",
    },
    CommandSpec {
        name: "kill-session",
        aliases: &["kills"],
        summary: "Kill a session",
        usage: "    lrmux kill-session -t <name>",
        body: "-t, --target <name>   Session name\n\nNot yet implemented from the CLI (use kill-server, or Ctrl-A K inside).",
    },
    CommandSpec {
        name: "kill-window",
        aliases: &["killw"],
        summary: "Kill a window",
        usage: "    lrmux kill-window -t <target>",
        body: "-t, --target <target>   Window to kill\n\nNot yet implemented from the CLI (use Ctrl-A x inside).",
    },
    CommandSpec {
        name: "select-window",
        aliases: &["selectw"],
        summary: "Select a window",
        usage: "    lrmux select-window -t <target>",
        body: "-t, --target <target>   Window to select\n\nNot yet implemented from the CLI (use Ctrl-A 0-9 or Ctrl-A n/p).",
    },
    CommandSpec {
        name: "rename-window",
        aliases: &["renamew"],
        summary: "Rename a window",
        usage: "    lrmux rename-window -t <target> <name>",
        body: "-t, --target <target>   Window to rename\n<name>                   New name\n\nNot yet implemented from the CLI.",
    },
    CommandSpec {
        name: "send-keys",
        aliases: &["send"],
        summary: "Send keys to a pane",
        usage: "    lrmux send-keys -t <target> <keys> [-q]",
        body: "\
-t, --target <target>   Pane to write\n\
-q, --quiet             Suppress stderr\n\
\n\
Special keys: C-c, C-a, F1-F12, Enter, Tab, Escape, Space, BS,\n\
Up, Down, Left, Right, Home, End, PageUp, PageDown, M-x.\n\
Anything else is sent as literal text.",
    },
    CommandSpec {
        name: "capture-pane",
        aliases: &["capturep", "capture-window"],
        summary: "Print a pane's contents",
        usage: "    \
lrmux capture-pane -t <target> [-p] [-c|--colors] [--format ascii|ansi|html|markdown] [--clipboard] [--file <path>]",
        body: "\
-t, --target <target>          Pane to capture\n\
-p, --print                    Print to stdout\n\
-c, --colors                   Include cell styles\n\
--format <ascii|ansi|html|markdown>\n\
--clipboard                    Copy to the system clipboard\n\
--file <path>                  Write to a file",
    },
    CommandSpec {
        name: "list-sessions",
        aliases: &["ls", "ls-sessions", "lss"],
        summary: "List sessions",
        usage: "    lrmux list-sessions [-s <server|host:port>]",
        body: "\
-s, --server <name|host:port>   One server, or a TCP address.\n\
                                With no -s, list local servers and LAN discovery.\n\
--tcp <host:port>               Query that endpoint.",
    },
    CommandSpec {
        name: "list-windows",
        aliases: &["lsw", "lswindow"],
        summary: "List windows in a session",
        usage: "    lrmux list-windows -t <session>",
        body: "-t, --target <session>   Session whose windows to list\n\nNot yet implemented.",
    },
    CommandSpec {
        name: "list-servers",
        aliases: &["ls-servers"],
        summary: "List running servers",
        usage: "    lrmux list-servers",
        body: "Local Unix sockets plus LAN servers that answer UDP discovery.",
    },
    CommandSpec {
        name: "list-peers",
        aliases: &["peers"],
        summary: "List known peers from a manager/directory",
        usage: "    lrmux list-peers [-s <name>] [--tcp <host:port>]",
        body: "\
Queries a manager (or a server with [peers] directory = true) for its\n    \
peer cache: known lrmux servers, their addresses, trust state, and\n    \
session names. Defaults to the local 'default' server; use --tcp to\n    \
query a remote manager.",
    },
    CommandSpec {
        name: "kill-server",
        aliases: &[],
        summary: "Stop a server",
        usage: "    lrmux kill-server -s <name|host:port>",
        body: "\
-s, --server <name|host:port>   Local server, or a TCP address to send KillServer to.\n\
--tcp <host:port>               Same, as a global connect address.",
    },
    CommandSpec {
        name: "discover",
        aliases: &[],
        summary: "Probe the LAN for servers",
        usage: "    lrmux discover",
        body: "Broadcast a UDP discovery probe and print every server that answers.",
    },
    CommandSpec {
        name: "session-selector",
        aliases: &["ss"],
        summary: "Open the session selector",
        usage: "    lrmux session-selector",
        body: "Always show the selector, even when only one session exists.\nNeeds an interactive terminal (not from inside a pane).",
    },
    CommandSpec {
        name: "psk",
        aliases: &[],
        summary: "Show, set, or generate the TCP pre-shared key",
        usage: "    lrmux psk [show|set <secret>|generate]",
        body: "\
show        Print a short preview of the configured PSK\n\
set         Save a PSK to ~/.config/lrmux/config.toml\n\
generate    Create a random PSK, save it, and print it once\n\
\n\
Remote clients need the same secret via config, LRMUX_PSK, or --psk.",
    },
    CommandSpec {
        name: "versions",
        aliases: &["version"],
        summary: "Show client and server versions",
        usage: "    lrmux versions\n    lrmux -v\n    lrmux --version",
        body: "\
`lrmux -v` and `lrmux --version` print only this binary's version.\n\
`lrmux versions` also queries every running local server.",
    },
    CommandSpec {
        name: "control-mode",
        aliases: &["control"],
        summary: "tmux control mode for iTerm2",
        usage: "    lrmux -CC [-s <server|host:port>]\n    lrmux control-mode [-s <server|host:port>]",
        body: "\
-s, --server <name|host:port>   Server to control.\n\
                                A host:port connects over TCP.\n\
\n\
Also: `lrmux attach -CC -s <server>`.",
    },
];

/// Command name aliases (tmux-compatible).
/// Maps short names to canonical names.
pub fn canonical_name(name: &str) -> &str {
    if let Some(spec) = lookup(name) {
        return spec.name;
    }
    // Control-mode names that are not top-level CLI commands.
    match name {
        "splitw" => "split-window",
        "detach" => "detach-client",
        "display" | "displayp" => "display-message",
        "show" => "show-option",
        "set" => "set-option",
        other => other,
    }
}

/// Look up a command by canonical name or alias.
pub fn lookup(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

/// `new-session` should create the session and return, not attach.
pub fn session_detached(parsed: &ParsedCmd) -> bool {
    parsed.has("d") || parsed.has("detach") || parsed.has("detached") || parsed.has("headless")
}

/// Index of commands plus the shared environment / prefix notes.
pub fn format_global_help() -> String {
    let mut s = String::from(
        "\
lrmux — a modern, fast terminal multiplexer\n\
\n\
USAGE:\n    \
lrmux [COMMAND] [OPTIONS]\n    \
lrmux <command> --help\n    \
lrmux help <command>\n\
\n\
COMMANDS:\n",
    );
    for c in COMMANDS {
        let alias = if c.aliases.is_empty() {
            String::new()
        } else {
            format!(" ({})", c.aliases.join(", "))
        };
        s.push_str(&format!("    {:<18} {}{alias}\n", c.name, c.summary));
    }
    s.push_str(
        "\n\
    With no command, lrmux attaches via the session selector\n    \
(auto-join when exactly one reachable session exists).\n    \
lrmux -- <cmd> [args]    Create a new window running <cmd> and attach\n\
\n\
GLOBAL OPTIONS:\n    \
    --tcp <host:port>    Connect over TCP (client commands: attach, ls, kill-server, …)\n    \
--psk <secret>       Pre-shared key for this process\n    \
-v, --version        Print this binary's version\n    \
-h, --help           This help, or `lrmux <command> --help` for one command\n\
\n\
On `new-server`, `--tcp` is the listen address, not a connect target.\n    \
`new-server --headless` (alias `start-server`) starts the server and returns.\n\
\n\
TARGETS (-t):\n    \
<session>             A session by name\n    \
<session>:<window>    A window by index in a session\n    \
:<window>             Window in the current session\n    \
(empty)               Current session/window\n\
\n\
NESTED USAGE:\n    \
Running lrmux inside lrmux creates a new window (like Ctrl-A c).\n    \
Running `lrmux new-session` inside lrmux creates a new session.\n    \
`lrmux new-server` and `lrmux ss` need an interactive terminal to attach;\n    \
`new-server` from inside a pane still forks the server and returns.\n\
\n\
ENVIRONMENT:\n    \
LRMUX_SYSLOG=host:port   Send logs to remote syslog (UDP RFC 3164)\n    \
LRMUX_LOG_LEVEL=debug|info|warn|error   Log level (default: info)\n    \
LRMUX=1                  Set automatically inside lrmux panes\n    \
LRMUX_SERVER=name        Server name (set automatically inside lrmux panes)\n    \
LRMUX_PSK=<secret>       TCP pre-shared key override\n\
\n\
PREFIX KEY: Ctrl-A (default)\n\
\n\
COMMON PREFIX COMMANDS:\n    \
Ctrl-A c    New window\n    \
Ctrl-A n/p  Next/prev window\n    \
Ctrl-A Ctrl-A  Toggle last window\n    \
Ctrl-A a    Send Ctrl-A to pane\n    \
Ctrl-A 0-9  Select window\n    \
Ctrl-A C    New session\n    \
Ctrl-A N/P  Next/prev session\n    \
Ctrl-A S    Session chooser\n    \
Ctrl-A $    Rename session\n    \
Ctrl-A [    Enter copy mode\n    \
Ctrl-A ]    Paste\n    \
Ctrl-A d    Detach\n    \
Ctrl-A x    Kill pane\n    \
Ctrl-A F    Resize to terminal\n    \
Ctrl-A K    Kill session\n    \
Ctrl-A \\    Show server log\n    \
Ctrl-A ?    Show keybindings\n\
\n\
LOGS:\n    \
File: /tmp/lrmux-<UID>/logs/<server>.log\n    \
State: /tmp/lrmux-<UID>/logs/<server>.state\n    \
Ring log: Ctrl-A \\ (in-session, last 500 entries)\n\
\n\
TCP / remote:\n    \
lrmux new-server --tcp <addr> [--headless]   Listen on TCP\n    \
lrmux attach <host:port>                      Attach over TCP\n    \
lrmux --tcp <addr> [--psk <s>]                Attach / CLI over TCP\n    \
lrmux discover / ls / list-servers            Local + LAN inventory\n    \
lrmux psk generate|set|show                   Share one secret\n    \
Config: ~/.config/lrmux/config.toml [network]\n    \
safe_networks = [] (default) => TLS required for all TCP peers\n",
    );
    s
}

/// Help for one command. Aliases say which command they stand for.
pub fn format_command_help(invoked: &str) -> Result<String, String> {
    let spec = lookup(invoked)
        .ok_or_else(|| format!("unknown command '{invoked}'. Try `lrmux --help`."))?;
    let mut s = String::new();
    if invoked != spec.name {
        if invoked == "start-server" {
            s.push_str("`start-server` is an alias of `new-server --headless`.\n\n");
        } else {
            s.push_str(&format!("`{invoked}` is an alias of `{}`.\n\n", spec.name));
        }
    }
    s.push_str(&format!(
        "lrmux {} — {}\n\nUSAGE:\n{}\n",
        spec.name, spec.summary, spec.usage
    ));
    if !spec.aliases.is_empty() {
        s.push_str("\nALIASES:\n    ");
        s.push_str(&spec.aliases.join(", "));
        s.push('\n');
    }
    if !spec.body.is_empty() {
        s.push('\n');
        s.push_str(spec.body);
        if !spec.body.ends_with('\n') {
            s.push('\n');
        }
    }
    s.push_str("\n-h, --help    Show this help\n");
    Ok(s)
}

/// Where an `attach` argument points.
#[derive(Debug, PartialEq, Eq)]
pub enum AttachTarget {
    /// Connect to this `host:port`. `session` is optional.
    Tcp {
        addr: String,
        session: Option<String>,
    },
    /// Local Unix socket. `server` None means the default server.
    Local {
        server: Option<String>,
        session: Option<String>,
    },
}

/// True when `s` should open a TCP connection.
///
/// Matches `<ipv4>:<port>`, `[<ipv6>]:<port>`, and `<dotted-host>:<port>`.
/// A dotted host is left as a local `server:session` target when `local_name_exists`
/// reports that the host part is already a running Unix server.
pub fn is_tcp_connect_target(s: &str, local_name_exists: impl Fn(&str) -> bool) -> bool {
    let Some((host, port)) = split_host_port(s) else {
        return false;
    };
    if !is_tcp_port(port) {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    host.contains('.') && !host.contains('/') && !local_name_exists(host)
}

/// Resolve attach's `-s` / `-t` / positionals into a TCP endpoint or a local server.
///
/// `first_pos` / `second_pos` are positional args. When `-t` is set, the first
/// positional is an extra session name after a TCP address given as `-t` or
/// the sole target; when `-t` is absent, the first positional is the target
/// and the second is the session (`attach <host:port> <session>`).
pub fn classify_attach(
    server_flag: Option<&str>,
    target_flag: Option<&str>,
    first_pos: Option<&str>,
    second_pos: Option<&str>,
    local_name_exists: impl Fn(&str) -> bool,
) -> AttachTarget {
    if let Some(srv) = server_flag {
        let session = target_flag.or(first_pos).map(|s| s.to_string());
        if is_tcp_connect_target(srv, &local_name_exists) {
            return AttachTarget::Tcp {
                addr: srv.to_string(),
                session,
            };
        }
        return AttachTarget::Local {
            server: Some(srv.to_string()),
            session,
        };
    }

    let target = target_flag.or(first_pos);
    let extra = if target_flag.is_some() {
        first_pos
    } else {
        second_pos
    };
    let Some(t) = target else {
        return AttachTarget::Local {
            server: None,
            session: None,
        };
    };
    if is_tcp_connect_target(t, &local_name_exists) {
        return AttachTarget::Tcp {
            addr: t.to_string(),
            session: extra.map(|s| s.to_string()),
        };
    }
    if let Some((srv, sess)) = t.split_once(':') {
        return AttachTarget::Local {
            server: if srv.is_empty() {
                None
            } else {
                Some(srv.to_string())
            },
            session: if sess.is_empty() {
                None
            } else {
                Some(sess.to_string())
            },
        };
    }
    if local_name_exists(t) {
        return AttachTarget::Local {
            server: Some(t.to_string()),
            session: extra.map(|s| s.to_string()),
        };
    }
    AttachTarget::Local {
        server: None,
        session: Some(t.to_string()),
    }
}

fn split_host_port(s: &str) -> Option<(&str, &str)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        if host.is_empty() {
            return None;
        }
        return Some((host, port));
    }
    let (host, port) = s.rsplit_once(':')?;
    if host.is_empty() || host.contains(':') {
        return None;
    }
    Some((host, port))
}

fn is_tcp_port(s: &str) -> bool {
    matches!(s.parse::<u16>(), Ok(p) if p > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_help_is_specific_and_aliases_share_it() {
        let global = format_global_help();
        let attach = format_command_help("attach").unwrap();
        let via_alias = format_command_help("attach-session").unwrap();
        assert!(global.contains("PREFIX KEY"));
        assert!(global.contains("attach"));
        assert!(!attach.contains("PREFIX KEY"));
        assert!(attach.contains("lrmux attach —"));
        assert!(attach.contains("<host:port>"));
        assert!(via_alias.contains("alias of `attach`"));
        assert!(via_alias.contains("lrmux attach —"));
        assert_ne!(attach, global);
        let start = format_command_help("start-server").unwrap();
        assert!(start.contains("alias of `new-server --headless`"));
        assert!(start.contains("--headless"));
        assert!(format_command_help("no-such-command").is_err());
    }

    #[test]
    fn ip_port_is_tcp_and_name_colon_stays_local() {
        let tcp = classify_attach(None, None, Some("10.17.17.16:17281"), None, |_| false);
        assert_eq!(
            tcp,
            AttachTarget::Tcp {
                addr: "10.17.17.16:17281".into(),
                session: None,
            }
        );
        let with_session =
            classify_attach(Some("10.17.17.16:17281"), Some("work"), None, None, |_| {
                false
            });
        assert_eq!(
            with_session,
            AttachTarget::Tcp {
                addr: "10.17.17.16:17281".into(),
                session: Some("work".into()),
            }
        );
        let positional_session =
            classify_attach(None, None, Some("10.17.17.16:17281"), Some("work"), |_| {
                false
            });
        assert_eq!(
            positional_session,
            AttachTarget::Tcp {
                addr: "10.17.17.16:17281".into(),
                session: Some("work".into()),
            }
        );
        let local = classify_attach(None, None, Some("nombre:sesion"), None, |_| false);
        assert_eq!(
            local,
            AttachTarget::Local {
                server: Some("nombre".into()),
                session: Some("sesion".into()),
            }
        );
        let existing = classify_attach(None, None, Some("nombre:17281"), None, |n| n == "nombre");
        assert_eq!(
            existing,
            AttachTarget::Local {
                server: Some("nombre".into()),
                session: Some("17281".into()),
            }
        );
        assert!(is_tcp_connect_target("[::1]:17281", |_| false));
        assert!(is_tcp_connect_target("host.example:17281", |_| false));
        assert!(!is_tcp_connect_target("host.example:17281", |n| {
            n == "host.example"
        }));
        assert!(!is_tcp_connect_target("nombre:sesion", |_| false));
    }

    #[test]
    fn headless_flag_does_not_swallow_the_next_argument() {
        let new_server = parse_flags(&[
            "--headless".into(),
            "--tcp".into(),
            "0.0.0.0:17281".into(),
            "-s".into(),
            "tcptest".into(),
        ]);
        assert!(new_server.has("headless"));
        assert_eq!(new_server.get("tcp"), Some("0.0.0.0:17281"));
        assert_eq!(new_server.get("s"), Some("tcptest"));

        let new_session = parse_flags(&["--headless".into(), "-s".into(), "work".into()]);
        assert!(session_detached(&new_session));
        assert_eq!(new_session.get("s"), Some("work"));

        let detached = parse_flags(&["-d".into(), "-s".into(), "work".into()]);
        assert!(session_detached(&detached));
        assert!(!detached.has("headless"));
    }

    #[test]
    fn control_mode_aliases_stay_canonical() {
        assert_eq!(canonical_name("attach"), "attach");
        assert_eq!(canonical_name("attach-session"), "attach");
        assert_eq!(canonical_name("start-server"), "new-server");
        assert_eq!(canonical_name("send"), "send-keys");
        assert_eq!(canonical_name("show"), "show-option");
        assert_eq!(canonical_name("new"), "new-session");
    }
}
