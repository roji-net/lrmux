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
                if matches!(rest, "detach" | "print" | "quiet" | "detached") {
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

/// Command name aliases (tmux-compatible).
/// Maps short names to canonical names.
pub fn canonical_name(name: &str) -> &str {
    match name {
        "new" => "new-session",
        "attach" => "attach-session",
        "ls" => "list-sessions",
        "lsw" | "lswindow" => "list-windows",
        "neww" => "new-window",
        "killw" => "kill-window",
        "selectw" => "select-window",
        "splitw" => "split-window",
        "renamew" => "rename-window",
        "send" => "send-keys",
        "capturep" => "capture-pane",
        "detach" => "detach-client",
        "display" | "displayp" => "display-message",
        "show" => "show-option",
        "set" => "set-option",
        "kills" => "kill-session",
        "lss" => "list-sessions",
        "ss" => "session-selector",
        other => other,
    }
}
