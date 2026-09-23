// Unified server/session inventory: local Unix sockets + LAN UDP discovery.
//
// Used by `list-servers`, `list-sessions`, `discover`, and the session selector
// so every entry point sees the same view of the world.

use std::io;
use std::time::Duration;

use crate::ipc;
use crate::proto::{self, ClientMsg, ServerMsg};

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
    pub sessions: Vec<String>,
    /// Primary address for display (Unix path or `host:port`).
    pub address: String,
    /// Set when the server answered discovery but ListSessions failed.
    pub probe_error: Option<String>,
}

impl ServerEntry {
    pub fn is_lan(&self) -> bool {
        matches!(self.source, ServerSource::Lan { .. })
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
            } => format!(
                "{} @ {} [lan] tls={} v={}",
                self.name, tcp_addr, tls, version
            ),
        }
    }
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
                // Skip duplicates of a local server that also announced (same name
                // already present as Local with reachable sessions).
                let already_local = entries
                    .iter()
                    .any(|e| e.name == ann.name && matches!(e.source, ServerSource::Local));
                if already_local {
                    continue;
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

fn query_sessions_unix(sock: &std::path::Path) -> io::Result<(String, Vec<String>)> {
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

fn query_sessions_tcp(addr: &str) -> io::Result<(String, Vec<String>)> {
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
