// IPC: Unix socket, TCP, TLS transport + UDP discovery.

pub mod discovery;
pub mod stream;
pub mod tls;
pub mod ws;

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub use stream::{ConnListener, ConnStream};

use crate::config::{NetworkConfig, TlsMode};

/// Optional TCP address override from `--tcp host:port`.
static TCP_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

/// Set/clear the global `--tcp` address used by clients and CLI helpers.
pub fn set_tcp_addr(addr: Option<String>) {
    *TCP_OVERRIDE.lock().unwrap() = addr;
}

/// Current `--tcp` override, if any.
pub fn tcp_addr() -> Option<String> {
    TCP_OVERRIDE.lock().unwrap().clone()
}

/// Connect using `--tcp` when set, otherwise the Unix socket at `path`.
pub fn connect_any(path: &Path) -> io::Result<ConnStream> {
    if let Some(addr) = tcp_addr() {
        return connect_tcp(&addr);
    }
    connect(path).map(ConnStream::Unix)
}

/// Compute the socket path for a given server name.
/// Format: /tmp/lrmux-<UID>/<server-name>
pub fn socket_path(server_name: &str) -> PathBuf {
    let uid = unsafe { libc::getuid() };
    let dir = format!("/tmp/lrmux-{uid}");
    Path::new(&dir).join(server_name)
}

/// Bind a Unix socket listener at the given path.
/// Only removes the socket file if it is genuinely stale (connect fails).
/// Never unlinks a socket that has a live listener — that would orphan a
/// running server and let a second server steal the path.
pub fn listen(path: &Path) -> io::Result<UnixListener> {
    // Create parent directory with 0700 permissions.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(parent, perms)?;
    }
    // Try binding first. If the path exists, this fails with EADDRINUSE.
    let old_umask = unsafe { libc::umask(0o077) };
    let listener = UnixListener::bind(path);
    unsafe { libc::umask(old_umask) };
    match listener {
        Ok(l) => Ok(l),
        Err(e) => {
            // Only remove the file if it's truly stale — a socket file that
            // refuses connections means no live listener owns it. If connect
            // succeeds, a live server owns this path; don't touch it.
            if path.exists() && UnixStream::connect(path).is_err() {
                std::fs::remove_file(path)?;
                let old_umask = unsafe { libc::umask(0o077) };
                let listener = UnixListener::bind(path);
                unsafe { libc::umask(old_umask) };
                listener
            } else {
                Err(e)
            }
        }
    }
}

/// Connect to a server at the given socket path.
pub fn connect(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path)
}

/// Connect to a server via TCP (optionally wrapping TLS per network policy).
pub fn connect_tcp(addr: &str) -> io::Result<ConnStream> {
    connect_tcp_with(addr, &crate::config::global().network)
}

/// Connect via TCP using an explicit network config (TLS policy / overrides).
pub fn connect_tcp_with(addr: &str, net: &NetworkConfig) -> io::Result<ConnStream> {
    let stream = std::net::TcpStream::connect(addr)?;
    let peer = stream.peer_addr().unwrap_or_else(|_| {
        // Fallback if peer_addr fails — treat as remote (require TLS on auto).
        "8.8.8.8:1".parse().unwrap()
    });
    let want_tls = tls::tls_required(net, peer) || matches!(net.tls, TlsMode::On);
    if want_tls {
        if matches!(net.tls, TlsMode::Off) {
            return Err(io::Error::other(
                "TLS required for this peer but network.tls = off",
            ));
        }
        let host = tls::host_from_addr(addr);
        let tls_stream = tls::wrap_client(stream, &host)?;
        Ok(ConnStream::TlsClient(Box::new(tls_stream)))
    } else {
        Ok(ConnStream::Tcp(stream))
    }
}

/// Bind a TCP listener at the given address.
pub fn listen_tcp(addr: &str) -> io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(addr)?;
    Ok(listener)
}

/// Wrap an accepted plain TCP stream in TLS if required by policy/config.
pub fn maybe_wrap_accepted_tcp(
    stream: ConnStream,
    net: &NetworkConfig,
    server_tls: Option<&std::sync::Arc<rustls::ServerConfig>>,
) -> io::Result<ConnStream> {
    let ConnStream::Tcp(tcp) = stream else {
        return Ok(stream);
    };
    let peer = tcp.peer_addr()?;
    let want_tls = tls::tls_required(net, peer) || matches!(net.tls, TlsMode::On);
    if !want_tls {
        return Ok(ConnStream::Tcp(tcp));
    }
    let Some(cfg) = server_tls else {
        return Err(io::Error::other(
            "TLS required for this peer but server has no TLS cert configured",
        ));
    };
    let tls_stream = tls::wrap_server(tcp, cfg.clone())?;
    Ok(ConnStream::TlsServer(Box::new(tls_stream)))
}

/// Check if a server is listening at the given path.
pub fn server_exists(path: &Path) -> bool {
    path.exists() && UnixStream::connect(path).is_ok()
}

/// Remove the socket file (cleanup on server exit).
pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Generate a unique server name by scanning existing sockets.
/// Returns "default" if free, otherwise "server-2", "server-3", etc.
pub fn auto_server_name() -> String {
    let uid = unsafe { libc::getuid() };
    let dir = format!("/tmp/lrmux-{uid}");
    let existing: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let base = "default";
    if !existing.iter().any(|n| n == base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("server-{n}");
        if !existing.iter().any(|name| name == &candidate) {
            return candidate;
        }
        n += 1;
    }
}
