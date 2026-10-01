// tmux command shim: when this binary runs with argv[0] == "tmux"
// (via the symlink installed by a tmux-compat server), translate the
// tmux command line into a single control-mode command to the server
// named by $TMUX (or -S/-L) and print the response on stdout.

use std::io;
use std::io::Write;

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};

/// tmux version reported by the shim (`tmux -V`) and exported to pane
/// children as `TERM_PROGRAM_VERSION` in tmux-compat mode.
pub const TMUX_VERSION: &str = "3.4";

/// Entry point. Returns a process exit code (0 ok, 1 tmux-style error).
pub fn run(args: &[String]) -> i32 {
    let mut socket: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-S" => {
                socket = args.get(i + 1).cloned();
                i += 2;
            }
            "-L" => {
                // tmux socket name → our socket dir layout.
                if let Some(name) = args.get(i + 1) {
                    let uid = unsafe { libc::getuid() };
                    socket = Some(format!("/tmp/lrmux-{uid}/{name}"));
                }
                i += 2;
            }
            "-f" | "-T" => i += 2,
            "-u" | "-l" | "-v" | "-VV" | "-x" => i += 1,
            "-V" | "--version" => {
                // Leading "tmux 3.4" keeps version parsers happy; the
                // suffix reveals this is lrmux's compat mode.
                println!(
                    "tmux {} (lrmux {} compat)",
                    TMUX_VERSION,
                    crate::version::VERSION
                );
                return 0;
            }
            _ => break,
        }
    }
    let cmd_args = &args[i..];
    // Bare `tmux -v` (no command) shows the version rather than tmux's
    // usage error — a convenient probe for "which tmux am I running".
    if cmd_args.is_empty() && args.iter().any(|a| a == "-v") {
        println!(
            "tmux {} (lrmux {} compat)",
            TMUX_VERSION,
            crate::version::VERSION
        );
        return 0;
    }
    if cmd_args.is_empty() {
        eprintln!("usage: tmux [-S socket] command [flags]");
        return 1;
    }
    if matches!(cmd_args[0].as_str(), "-h" | "--help" | "help") {
        println!(
            "tmux compatibility shim (lrmux {})\n\
             \n\
             usage: tmux [-S socket] command [flags]\n\
             \n\
             Supported: new-window kill-window display-message \\\n\
             set-window-option\n\
             show-environment list-sessions list-windows\n\
             \n\
             Other commands return success with empty output.",
            crate::version::VERSION
        );
        return 0;
    }

    // Socket: -S/-L wins, then $TMUX's first field.
    let socket = socket.or_else(|| {
        std::env::var("TMUX")
            .ok()
            .and_then(|t| t.split(',').next().map(str::to_string))
    });
    let Some(socket) = socket else {
        eprintln!("no server running");
        return 1;
    };

    // The session this command targets: $TMUX's third field ($N).
    let session_id = std::env::var("TMUX")
        .ok()
        .and_then(|t| t.split(',').nth(2).map(str::to_string));

    match dispatch(&socket, session_id.as_deref(), cmd_args) {
        Ok(true) => 0,
        Ok(false) => 1, // server answered %error (payload already on stderr)
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn dispatch(socket: &str, session_id: Option<&str>, cmd_args: &[String]) -> io::Result<bool> {
    let mut stream = ipc::connect_any(std::path::Path::new(socket))
        .map_err(|e| io::Error::new(e.kind(), format!("error connecting to {socket} ({e})")))?;

    let msg = proto::encode_client(&ClientMsg::IdentifyControl {
        rows: 24,
        cols: 80,
        auth_token: crate::config::effective_psk(),
    });
    proto::send(&mut stream, &msg)?;

    // Wait for IdentifyAck (or an error) before sending the command.
    let mut pending_error: Option<String> = None;
    loop {
        match proto::decode_server(&mut stream) {
            Ok(ServerMsg::IdentifyAck { .. }) => break,
            Ok(ServerMsg::Error { msg }) => {
                pending_error = Some(msg);
                break;
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    if let Some(e) = pending_error {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, e));
    }

    // Reassemble a tmux-style command line for the control-mode parser.
    // Quote args so names/commands with spaces survive as one token.
    let mut line = shell_quote(&cmd_args[0]);
    for a in &cmd_args[1..] {
        line.push(' ');
        line.push_str(&shell_quote(a));
    }

    // Target the caller's session when the command accepts -t and none
    // was given — a control client's default session would otherwise
    // be session 0 regardless of where the shim's pane lives.
    let cmd = crate::cmd::canonical_name(&cmd_args[0]);
    if let Some(sid) = session_id
        && !cmd_args.iter().any(|a| a == "-t" || a.starts_with("-t"))
        && matches!(cmd, "new-window" | "display-message")
    {
        // TMUX's third field is a bare id; -t wants tmux's $N form.
        line.push_str(&format!(" -t '${sid}'"));
    }

    let msg = proto::encode_client(&ClientMsg::ControlCommand { line });
    proto::send(&mut stream, &msg)?;

    // Control responses are %begin ts seq flags … payload … %end|%error.
    // The block answering a client command has flags&1; server-originated
    // blocks (initial state) and %notifications outside blocks are noise.
    let mut collecting = false;
    let mut out: Vec<String> = Vec::new();
    loop {
        let msg = proto::decode_server(&mut stream)?;
        let line = match msg {
            ServerMsg::ControlNotify { line } => line,
            ServerMsg::Error { msg } => {
                return Err(io::Error::other(msg));
            }
            _ => continue,
        };
        if let Some(rest) = line.strip_prefix("%begin ") {
            let flags: u32 = rest
                .split_whitespace()
                .nth(2)
                .and_then(|f| f.parse().ok())
                .unwrap_or(0);
            collecting = flags & 1 == 1;
            out.clear();
        } else if line.starts_with("%end ") || line.starts_with("%error ") {
            if collecting {
                let failed = line.starts_with("%error");
                let stdout = io::stdout();
                let stderr = io::stderr();
                for l in &out {
                    if failed {
                        let _ = writeln!(stderr.lock(), "{l}");
                    } else {
                        let _ = writeln!(stdout.lock(), "{l}");
                    }
                }
                return Ok(!failed);
            }
        } else if collecting && !line.starts_with('%') {
            out.push(line);
        }
    }
}

/// Quote one argument for the server's control-command parser:
/// single-quote anything unsafe, with '\'' escaping inside.
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%_+=:,./-#{}".contains(c))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_roundtrip() {
        // Safe args pass through; anything else gets single-quoted.
        assert_eq!(shell_quote("@5"), "@5");
        assert_eq!(shell_quote("#{window_id}"), "#{window_id}");
        assert_eq!(shell_quote("-F"), "-F");
        assert_eq!(shell_quote("my window"), "'my window'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
