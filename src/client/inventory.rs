// Unified server/session inventory: local Unix sockets + LAN UDP discovery.
//
// Used by `list-servers`, `list-sessions`, `discover`, and the session selector
// so every entry point sees the same view of the world.

use std::collections::HashSet;
use std::io;
use std::sync::OnceLock;
use std::time::Duration;

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg, SessionInfo};

/// Where a server was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSource {
    /// Local Unix socket under `/tmp/lrmux-<UID>/`.
    Local,
    /// Answered a UDP discovery probe on the LAN.
    Lan {
        tcp_addr: String,
        tls: bool,
        version: String,
        fingerprint: String,
    },
}

/// One server plus its sessions.
#[derive(Debug, Clone)]
pub struct ServerEntry {
    pub name: String,
    pub source: ServerSource,
    pub sessions: Vec<SessionInfo>,
    /// Primary address for display (Unix path or `host:port`).
    pub address: String,
    /// Set when the server answered discovery but ListSessions failed.
    pub probe_error: Option<String>,
}

impl ServerEntry {
    pub fn is_lan(&self) -> bool {
        matches!(self.source, ServerSource::Lan { .. })
    }

    /// True for Unix-socket servers, and for LAN announcements whose TCP
    /// address is this machine (loopback or a local interface IP).
    pub fn is_this_machine(&self) -> bool {
        match &self.source {
            ServerSource::Local => true,
            ServerSource::Lan { tcp_addr, .. } => is_local_host(&host_of_addr(tcp_addr)),
        }
    }

    pub fn tcp_addr(&self) -> Option<&str> {
        match &self.source {
            ServerSource::Lan { tcp_addr, .. } => Some(tcp_addr.as_str()),
            ServerSource::Local => None,
        }
    }

    /// Label for CLI / selector rows.
    pub fn display_label(&self) -> String {
        match &self.source {
            ServerSource::Local => format!("{} @ {} [local]", self.name, self.address),
            ServerSource::Lan {
                tcp_addr,
                tls,
                version,
                ..
            } => {
                let tag = if is_local_host(&host_of_addr(tcp_addr)) {
                    "local"
                } else {
                    "lan"
                };
                format!(
                    "{} @ {} [{tag}] tls={} v={}",
                    self.name, tcp_addr, tls, version
                )
            }
        }
    }
}

/// Host part of `host:port` or `[ipv6]:port`.
pub fn host_of_addr(addr: &str) -> String {
    split_host_port(addr).0
}

/// Port part of `host:port` or `[ipv6]:port`, if present.
pub fn port_of_addr(addr: &str) -> Option<String> {
    split_host_port(addr).1
}

fn split_host_port(addr: &str) -> (String, Option<String>) {
    if let Some(rest) = addr.strip_prefix('[')
        && let Some((host, port)) = rest.split_once("]:")
    {
        return (format!("[{host}]"), Some(port.to_string()));
    }
    if let Some((host, port)) = addr.rsplit_once(':')
        && !port.is_empty()
        && port.chars().all(|c| c.is_ascii_digit())
    {
        return (host.to_string(), Some(port.to_string()));
    }
    (addr.to_string(), None)
}

/// True if `host` is loopback or one of this machine's interface addresses.
pub fn is_local_host(host: &str) -> bool {
    let h = host.trim_matches(['[', ']']);
    if h.eq_ignore_ascii_case("localhost") || h == "127.0.0.1" || h == "::1" {
        return true;
    }
    // IPv4 loopback net
    if let Ok(ip) = h.parse::<std::net::Ipv4Addr>()
        && ip.is_loopback()
    {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::Ipv6Addr>()
        && ip.is_loopback()
    {
        return true;
    }
    local_interface_ips().contains(h)
}

fn local_interface_ips() -> &'static HashSet<String> {
    static IPS: OnceLock<HashSet<String>> = OnceLock::new();
    IPS.get_or_init(|| {
        let mut set = HashSet::new();
        unsafe {
            let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut ifap) != 0 {
                return set;
            }
            let mut ifa = ifap;
            while !ifa.is_null() {
                let addr = (*ifa).ifa_addr;
                if !addr.is_null() {
                    match i32::from((*addr).sa_family) {
                        libc::AF_INET => {
                            let sin = addr as *const libc::sockaddr_in;
                            let ip = std::net::Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
                            set.insert(ip.to_string());
                        }
                        libc::AF_INET6 => {
                            let sin6 = addr as *const libc::sockaddr_in6;
                            let ip = std::net::Ipv6Addr::from((*sin6).sin6_addr.s6_addr);
                            set.insert(ip.to_string());
                        }
                        _ => {}
                    }
                }
                ifa = (*ifa).ifa_next;
            }
            libc::freeifaddrs(ifap);
        }
        set
    })
}

/// Scan local sockets and probe the LAN (UDP discovery). Always probes, even
/// when the local config has `discovery = false` — that flag only controls
/// whether *this* server announces.
pub fn collect(discover_timeout: Duration) -> io::Result<Vec<ServerEntry>> {
    let mut entries: Vec<ServerEntry> = Vec::new();

    // Local Unix sockets.
    let uid = unsafe { libc::getuid() };
    let dir = format!("/tmp/lrmux-{uid}");
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        let mut names: Vec<String> = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            if ipc::server_exists(&path)
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                // Skip non-socket noise (logs/, state files).
                if name == "logs" {
                    continue;
                }
                names.push(name.to_string());
            }
        }
        names.sort();
        for name in names {
            let sock = ipc::socket_path(&name);
            match query_sessions_unix(&sock) {
                Ok((address, sessions)) => {
                    entries.push(ServerEntry {
                        name: name.clone(),
                        source: ServerSource::Local,
                        sessions,
                        address,
                        probe_error: None,
                    });
                }
                Err(e) => {
                    eprintln!("lrmux: local server '{name}' is not responding ({e})");
                }
            }
        }
    }

    // LAN discovery.
    let port = crate::config::global().network.discovery_port;
    match ipc::discovery::discover(port, discover_timeout) {
        Ok(anns) => {
            for ann in anns {
                let tcp = ann.tcp_addr();
                let host = host_of_addr(&tcp);
                let on_this_machine = is_local_host(&host);
                // Skip duplicate views of the same server:
                // - Unix socket already listed, or
                // - another announce from this machine (127.0.0.1 vs LAN IP).
                let already = entries.iter().any(|e| e.name == ann.name);
                if already {
                    let have_unix = entries.iter().any(|e| e.name == ann.name && !e.is_lan());
                    if on_this_machine || have_unix {
                        continue;
                    }
                }
                match query_sessions_tcp(&tcp) {
                    Ok((address, sessions)) => {
                        entries.push(ServerEntry {
                            name: ann.name,
                            source: ServerSource::Lan {
                                tcp_addr: tcp,
                                tls: ann.tls,
                                version: ann.version,
                                fingerprint: ann.fingerprint,
                            },
                            sessions,
                            address,
                            probe_error: None,
                        });
                    }
                    Err(e) => {
                        // Still list the announcement so the user sees it.
                        // The selector must not treat this as "no sessions"
                        // and fork a local server.
                        eprintln!(
                            "lrmux: lan server '{}' @ {} listed but ListSessions failed ({e})",
                            ann.name, tcp
                        );
                        entries.push(ServerEntry {
                            name: ann.name,
                            source: ServerSource::Lan {
                                tcp_addr: tcp.clone(),
                                tls: ann.tls,
                                version: ann.version,
                                fingerprint: ann.fingerprint,
                            },
                            sessions: vec![],
                            address: tcp,
                            probe_error: Some(e.to_string()),
                        });
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("lrmux: warning: UDP discovery failed ({e})");
        }
    }

    Ok(entries)
}

fn query_sessions_unix(sock: &std::path::Path) -> io::Result<(String, Vec<SessionInfo>)> {
    let mut stream = ipc::connect(sock).map(ipc::ConnStream::Unix)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let msg = proto::encode_client(&ClientMsg::ListSessions);
    proto::send(&mut stream, &msg)?;
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::SessionList { sessions, address }) => Ok((address, sessions)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected SessionList",
        )),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            Err(io::Error::new(e.kind(), "timed out"))
        }
        Err(e) => Err(e),
    }
}

fn query_sessions_tcp(addr: &str) -> io::Result<(String, Vec<SessionInfo>)> {
    let mut stream = ipc::connect_tcp(addr)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let msg = proto::encode_client(&ClientMsg::ListSessions);
    proto::send(&mut stream, &msg)?;
    match proto::decode_server(&mut stream) {
        Ok(ServerMsg::SessionList { sessions, address }) => Ok((address, sessions)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected SessionList",
        )),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            Err(io::Error::new(e.kind(), "timed out"))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_local() {
        assert!(is_local_host("127.0.0.1"));
        assert!(is_local_host("::1"));
        assert!(is_local_host("localhost"));
        assert!(is_local_host("[::1]"));
    }

    #[test]
    fn host_port_split() {
        assert_eq!(host_of_addr("10.0.0.1:9999"), "10.0.0.1");
        assert_eq!(port_of_addr("10.0.0.1:9999").as_deref(), Some("9999"));
        assert_eq!(host_of_addr("[::1]:17281"), "[::1]");
        assert_eq!(port_of_addr("[::1]:17281").as_deref(), Some("17281"));
    }
}
