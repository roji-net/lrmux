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

/// Optional relay (`--via host:port`): every TCP connect is tunnelled
/// through that manager's RelayOpen byte pipe instead of dialing the
/// target directly.
static VIA_ADDR: Mutex<Option<String>> = Mutex::new(None);

/// Set/clear the global `--via` relay address.
pub fn set_via_addr(addr: Option<String>) {
    *VIA_ADDR.lock().unwrap() = addr;
}

/// Current `--via` relay address, if any.
pub fn via_addr() -> Option<String> {
    VIA_ADDR.lock().unwrap().clone()
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

/// Connect via TCP with a bounded connect() — dead hosts fail fast
/// instead of riding the kernel SYN timeout. Used by peer polling,
/// where a stall would freeze the caller's event loop.
pub fn connect_tcp_timeout(addr: &str, timeout: std::time::Duration) -> io::Result<ConnStream> {
    connect_tcp_inner(addr, &crate::config::global().network, Some(timeout))
}

/// Connect via TCP using an explicit network config (TLS policy / overrides).
pub fn connect_tcp_with(addr: &str, net: &NetworkConfig) -> io::Result<ConnStream> {
    connect_tcp_inner(addr, net, None)
}

fn connect_tcp_inner(
    addr: &str,
    net: &NetworkConfig,
    timeout: Option<std::time::Duration>,
) -> io::Result<ConnStream> {
    // Relayed connect: TCP to the manager, Identify+RelayOpen, and the
    // socket becomes a raw pipe to `addr` from then on.
    let stream = if let Some(via) = via_addr() {
        relay_open(
            &via,
            addr,
            timeout.unwrap_or(std::time::Duration::from_secs(5)),
        )?
    } else {
        match timeout {
            Some(t) => {
                use std::net::ToSocketAddrs;
                let sa = addr
                    .to_socket_addrs()?
                    .next()
                    .ok_or_else(|| io::Error::other(format!("no address for {addr}")))?;
                std::net::TcpStream::connect_timeout(&sa, t)?
            }
            None => match std::net::TcpStream::connect(addr) {
                Ok(s) => s,
                // Auto-fallback: a direct connect that fails retries through
                // each configured manager — the manager may reach networks
                // the client cannot.
                Err(e) => {
                    let mut last = e;
                    let mut ok = None;
                    for m in &crate::config::global().peers.managers {
                        match relay_open(m, addr, std::time::Duration::from_secs(5)) {
                            Ok(s) => {
                                ok = Some(s);
                                break;
                            }
                            Err(re) => last = re,
                        }
                    }
                    match ok {
                        Some(s) => s,
                        None => return Err(last),
                    }
                }
            },
        }
    };
    let peer = if via_addr().is_some() {
        // Relayed: peer_addr() is the manager. The TLS policy applies to
        // the *target* — resolve its address for the decision instead.
        use std::net::ToSocketAddrs;
        addr.to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .unwrap_or_else(|| "8.8.8.8:1".parse().unwrap())
    } else {
        stream.peer_addr().unwrap_or_else(|_| {
            // Fallback if peer_addr fails — treat as remote (require TLS on auto).
            "8.8.8.8:1".parse().unwrap()
        })
    };
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

/// Open a raw byte pipe to `target` through a manager at `via`:
/// connect TCP to the manager, authenticate with Identify, send
/// RelayOpen, and wait for RelayAck{ok}. The returned stream then
/// carries opaque bytes to the target — the caller may run its own
/// TLS handshake over it for end-to-end encryption.
///
/// The manager connection itself is plaintext TCP; authentication is
/// the configured PSK. (Nested TLS to the manager is not supported —
/// TLS terminates at the target.)
fn relay_open(
    via: &str,
    target: &str,
    timeout: std::time::Duration,
) -> io::Result<std::net::TcpStream> {
    use crate::proto::{ClientMsg, ServerMsg};

    use std::net::ToSocketAddrs;
    let sa = via
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other(format!("no address for {via}")))?;
    let mut stream = ConnStream::Tcp(std::net::TcpStream::connect_timeout(&sa, timeout)?);

    // Identify (auth) so the relay request rides an authenticated conn.
    let ident = crate::proto::encode_client(&ClientMsg::Identify {
        rows: 0,
        cols: 0,
        attach: false,
        auth_token: crate::config::effective_psk(),
    });
    crate::proto::send(&mut stream, &ident)?;
    match stream::decode_with_deadline(&mut stream, timeout, |r| crate::proto::decode_server(r))? {
        ServerMsg::IdentifyAck { .. } => {}
        ServerMsg::Error { msg } => {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected IdentifyAck",
            ));
        }
    }

    let open = crate::proto::encode_client(&ClientMsg::RelayOpen {
        addr: target.to_string(),
    });
    crate::proto::send(&mut stream, &open)?;
    match stream::decode_with_deadline(&mut stream, timeout, |r| crate::proto::decode_server(r))? {
        ServerMsg::RelayAck { ok: true, .. } => {}
        ServerMsg::RelayAck { reason, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("relay {via}: {reason}"),
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected RelayAck",
            ));
        }
    }

    let ConnStream::Tcp(tcp) = stream else {
        return Err(io::Error::other("relay stream is not plain TCP"));
    };
    Ok(tcp)
}

/// Bind a TCP listener.
///
/// `auto` and a trailing `+` (`0.0.0.0:17280+`) try that port and then the
/// next ones until one is free. A plain `host:port` binds exactly that address.
pub fn listen_tcp(addr: &str) -> io::Result<std::net::TcpListener> {
    let addr = addr.trim();
    if addr.eq_ignore_ascii_case("auto") {
        return listen_tcp_first_free("0.0.0.0", DEFAULT_TCP_PORT);
    }
    if let Some(base) = addr.strip_suffix('+') {
        let (host, port) = split_bind_host_port(base.trim())?;
        return listen_tcp_first_free(&host, port);
    }
    std::net::TcpListener::bind(addr)
}

/// First port tried by `tcp_listen = "auto"` / `0.0.0.0:17280+`.
pub const DEFAULT_TCP_PORT: u16 = 17280;

fn listen_tcp_first_free(host: &str, start: u16) -> io::Result<std::net::TcpListener> {
    const SPAN: u32 = 1024;
    if start == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TCP port scan needs a start port greater than 0",
        ));
    }
    let last = (start as u32 + SPAN - 1).min(u16::MAX as u32);
    let mut in_use = None;
    for port in start as u32..=last {
        let candidate = format!("{host}:{port}");
        match std::net::TcpListener::bind(&candidate) {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => in_use = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(in_use.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("no free TCP port on {host} from {start} to {last}"),
        )
    }))
}

fn split_bind_host_port(addr: &str) -> io::Result<(String, u16)> {
    if let Some(rest) = addr.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid TCP address '{addr}+'"),
            )
        })?;
        let port = parse_port(port)?;
        return Ok((format!("[{host}]"), port));
    }
    let (host, port) = addr.rsplit_once(':').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid TCP address '{addr}+' (want host:port+)"),
        )
    })?;
    if host.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid TCP address '{addr}+'"),
        ));
    }
    Ok((host.to_string(), parse_port(port)?))
}

fn parse_port(port: &str) -> io::Result<u16> {
    port.parse::<u16>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid TCP port '{port}'"),
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_suffix_skips_a_busy_port() {
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let next = listen_tcp(&format!("127.0.0.1:{port}+")).unwrap();
        let got = next.local_addr().unwrap().port();
        assert!(got > port, "bound {got}, busy was {port}");
    }

    #[test]
    fn exact_address_does_not_scan() {
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let err = listen_tcp(&format!("127.0.0.1:{port}")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }
}
