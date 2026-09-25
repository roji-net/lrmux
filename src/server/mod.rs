// Server process: event loop, state, session/window/pane management.

mod capture;
mod event_loop;
mod pane;
mod session;
mod state;
mod window;

pub use capture::CaptureFormat;

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use rustls::ServerConfig;

use crate::config::{NetworkConfig, TlsMode};
use crate::ipc;
use crate::log;

/// The server name (socket file name), set once at startup.
/// Used by PTY spawn to set LRMUX_SERVER in the child environment.
static SERVER_NAME: OnceLock<String> = OnceLock::new();

/// The server primary address (TCP if any, otherwise the Unix socket path).
static SERVER_ADDRESS: OnceLock<String> = OnceLock::new();

/// Network settings active for this server process.
static NETWORK: OnceLock<NetworkConfig> = OnceLock::new();

/// Runtime PSK (hot-updatable via SetPsk). Initialized from config.
static RUNTIME_PSK: OnceLock<Mutex<String>> = OnceLock::new();

/// Optional rustls server config for TCP TLS wrapping.
static TLS_SERVER: OnceLock<Option<Arc<ServerConfig>>> = OnceLock::new();

/// TLS cert fingerprint advertised in discovery Announce packets.
static TLS_FINGERPRINT: OnceLock<String> = OnceLock::new();

/// Get the server name (for child process env vars).
pub fn server_name() -> &'static str {
    SERVER_NAME.get().map(|s| s.as_str()).unwrap_or("default")
}

/// Get the server primary address (TCP or Unix socket).
pub fn server_address() -> &'static str {
    SERVER_ADDRESS
        .get()
        .map(|s| s.as_str())
        .unwrap_or("unknown")
}

pub fn network() -> &'static NetworkConfig {
    NETWORK.get_or_init(NetworkConfig::default)
}

/// Current PSK required for TCP Identify (may change at runtime).
pub fn runtime_psk() -> String {
    RUNTIME_PSK
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap()
        .clone()
}

/// Hot-update the PSK for subsequent TCP handshakes.
pub fn set_runtime_psk(psk: String) {
    let cell = RUNTIME_PSK.get_or_init(|| Mutex::new(String::new()));
    *cell.lock().unwrap() = psk;
}

pub fn tls_server_config() -> Option<&'static Arc<ServerConfig>> {
    TLS_SERVER.get().and_then(|o| o.as_ref())
}

pub fn tls_fingerprint() -> &'static str {
    TLS_FINGERPRINT.get().map(|s| s.as_str()).unwrap_or("")
}

/// Start the server: bind the socket, run the event loop.
/// If `tcp_addr` / `ws_addr` are provided (CLI), they override
/// `network.tcp_listen` / `network.ws_listen`.
/// If `headless` is true, create a default session without waiting for
/// the first client (used by `lrmux new-server --headless` / `start-server`).
///
/// Optional bootstrap env (set by the parent before fork, cleared here):
/// - `LRMUX_INIT_COMMAND` — first window runs `$SHELL -ci <command>`
/// - `LRMUX_INIT_SESSION` — session name override
/// - `LRMUX_INIT_CWD` — cwd for the first window (client's cwd, or `-c`)
///
/// `TERM` / `COLORTERM` are inherited from the forking client as-is.
/// `manager` runs the session-manager role: no sessions, a peer
/// directory, open registrations, and the process stays alive with no
/// sessions or clients attached.
pub fn run(
    socket_path: &Path,
    tcp_addr: Option<&str>,
    ws_addr: Option<&str>,
    headless: bool,
    manager: bool,
) -> io::Result<()> {
    let net = crate::config::global().network.clone();
    let _ = NETWORK.set(net.clone());
    let _ = RUNTIME_PSK.set(Mutex::new(net.psk_value().to_string()));

    let tcp = tcp_addr
        .map(|s| s.to_string())
        .or_else(|| net.tcp_listen_addr().map(|s| s.to_string()));
    let ws = ws_addr
        .map(|s| s.to_string())
        .or_else(|| net.ws_listen_addr().map(|s| s.to_string()));

    // When TCP is enabled and discovery is not explicitly configured elsewhere,
    // prefer announcing if discovery=true OR tcp was requested (remote use).
    // Actual announce still requires discovery_sock below.

    // Prepare TLS certs whenever TCP is enabled and TLS is not forced off.
    // With default safe_networks=[] and tls=auto, TLS is required for all peers.
    // WebSocket does not use rustls here — terminate TLS at a reverse proxy (WSS).
    let mut fingerprint = String::new();
    let tls_cfg = if tcp.is_some() && !matches!(net.tls, TlsMode::Off) {
        match ipc::tls::ensure_server_certs(&net.tls_cert, &net.tls_key) {
            Ok((cert_path, key_path)) => {
                fingerprint = ipc::tls::cert_fingerprint_hex(&cert_path).unwrap_or_default();
                match ipc::tls::load_server_config(&cert_path, &key_path) {
                    Ok(cfg) => Some(cfg),
                    Err(e) => {
                        eprintln!("lrmux: warning: TLS cert load failed ({e}); TLS unavailable");
                        None
                    }
                }
            }
            Err(e) => {
                eprintln!("lrmux: warning: TLS cert bootstrap failed ({e}); TLS unavailable");
                None
            }
        }
    } else {
        None
    };
    let _ = TLS_SERVER.set(tls_cfg);
    let _ = TLS_FINGERPRINT.set(fingerprint);

    let unix_listener = ipc::listen(socket_path)?;

    // Build the listeners list (Unix + optional TCP + optional WebSocket).
    let mut listeners: Vec<ipc::ConnListener> = vec![ipc::ConnListener::Unix(unix_listener)];
    let mut tcp_bound: Option<String> = None;
    if let Some(ref addr) = tcp {
        let tcp_listener = ipc::listen_tcp(addr)?;
        let bound = tcp_listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| addr.clone());
        log::info(&format!("server also listening on TCP {bound}"));
        eprintln!("lrmux: server listening on TCP {bound}");
        tcp_bound = Some(bound);
        listeners.push(ipc::ConnListener::Tcp(tcp_listener));
    }

    if let Some(ref addr) = ws {
        let ws_listener = ipc::listen_tcp(addr)?;
        log::info(&format!("server WebSocket listening on {addr}"));
        eprintln!(
            "lrmux: WebSocket listening on ws://{addr} (serve web/ over HTTP; use a proxy for WSS)"
        );
        listeners.push(ipc::ConnListener::Ws(ws_listener));
    }

    // Enable discovery when configured, or automatically when TCP/WS is on
    // (so remote peers can find this server without extra flags).
    let want_discovery = net.discovery || tcp.is_some() || ws.is_some();
    let discovery_sock = if want_discovery {
        match ipc::discovery::bind_server(net.discovery_port) {
            Ok(s) => {
                eprintln!(
                    "lrmux: discovery enabled on UDP port {}",
                    net.discovery_port
                );
                Some(s)
            }
            Err(e) => {
                eprintln!("lrmux: warning: discovery bind failed ({e})");
                None
            }
        }
    } else {
        None
    };

    // Initialize logging.
    let uid = unsafe { libc::getuid() };
    let log_dir = format!("/tmp/lrmux-{uid}/logs");
    let server_name = socket_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    let _ = SERVER_NAME.set(server_name.to_string());
    let _ =
        SERVER_ADDRESS.set(tcp_bound.unwrap_or_else(|| socket_path.to_string_lossy().into_owned()));
    let syslog = std::env::var("LRMUX_SYSLOG").ok().and_then(|s| {
        let parts: Vec<&str> = s.rsplitn(2, ':').collect();
        if parts.len() == 2 {
            Some((parts[1].to_string(), parts[0].parse::<u16>().ok()?))
        } else {
            None
        }
    });
    let log_level = match std::env::var("LRMUX_LOG_LEVEL").as_deref() {
        Ok("debug") => log::Level::Debug,
        Ok("warn") => log::Level::Warn,
        Ok("error") => log::Level::Error,
        _ => log::Level::Info,
    };
    log::init(&log_dir, server_name, log_level, syslog);
    log::install_panic_hook();

    // Check for stale state file (indicates a previous crash).
    if let Some(stale_state) = state::check_stale_state(socket_path) {
        log::warn("previous server did not shut down cleanly — stale state file found");
        log::warn(&format!("previous state:\n{stale_state}"));
        eprintln!(
            "lrmux: WARNING — previous server may have crashed. Previous state:\n{stale_state}"
        );
        eprintln!("lrmux: check log file at {log_dir}/{server_name}.log for details.");
    }

    log::info(&format!("server starting on {}", socket_path.display()));
    eprintln!("lrmux: server listening on {}", socket_path.display());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        event_loop::run(listeners, socket_path, headless, manager, discovery_sock)
    }));

    match result {
        Ok(inner) => {
            if inner.is_ok() {
                state::cleanup_state(socket_path);
                log::info("server stopped cleanly.");
            } else {
                log::error(&format!(
                    "server stopped with error: {:?}",
                    inner.as_ref().err()
                ));
            }
            inner
        }
        Err(_) => {
            log::error("PANIC in event loop, exiting");
            eprintln!("lrmux: PANIC in event loop. State saved. Check log for details.");
            Err(io::Error::other("event loop panic"))
        }
    }
}
