// Peer cache: the directory role's view of known lrmux servers.
//
// Owned by the SessionManager (or a server with the directory role on).
// Entries are keyed by the stable server_id (UUID) — TCP ports are
// auto-assigned and change across restarts. Persisted as TOML at
// ~/.config/lrmux/peers.toml with absolute epoch timestamps so TTLs
// survive process restarts.
//
// Trust states (announces are unauthenticated and spoofable):
//   announced → (TCP verify ok) → verified → (poll fails) → stale
//   stale/expired entries are evicted once now > expires_at.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ipc::discovery::Announcement;

/// Default peer entry TTL (seconds).
pub const DEFAULT_TTL_SECS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerState {
    /// Seen via announce/register; not yet verified over TCP.
    Announced,
    /// TCP poll succeeded and the fingerprint matched.
    Verified,
    /// Last refresh poll failed — kept for display, flagged unreachable.
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerSource {
    Broadcast,
    Scan,
    Register,
    Gossip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEntry {
    pub server_id: String,
    pub name: String,
    /// Source IPs this id has been seen from (a node may roam).
    #[serde(default)]
    pub addrs: Vec<String>,
    pub tcp_port: u16,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub fingerprint: String,
    pub state: PeerState,
    /// Our PSK was accepted by this peer on the last verified poll.
    #[serde(default)]
    pub psk_ok: bool,
    /// Session names from the last successful ListSessions poll.
    #[serde(default)]
    pub sessions: Vec<String>,
    pub source: PeerSource,
    /// Absolute epoch seconds.
    pub last_seen: u64,
    pub expires_at: u64,
    /// Next scheduled verification poll (absolute epoch seconds).
    #[serde(default)]
    pub poll_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct PeerFile {
    #[serde(default)]
    peer: Vec<PeerEntry>,
}

pub struct PeerCache {
    ttl_secs: u64,
    peers: BTreeMap<String, PeerEntry>,
    dirty: bool,
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// u64 in [lo, hi] — jitter for refresh polls (avoids herd sync).
fn rand_range(lo: u64, hi: u64) -> u64 {
    let mut b = [0u8; 8];
    let span = hi.saturating_sub(lo) + 1;
    if crate::config::fill_random(&mut b).is_err() || span == 0 {
        return lo;
    }
    lo + u64::from_le_bytes(b) % span
}

impl PeerCache {
    /// Load the persisted cache from the default path, dropping expired
    /// entries. Missing/corrupt files yield an empty cache.
    pub fn load(ttl_secs: u64) -> Self {
        Self::load_from(&peers_path(), ttl_secs)
    }

    fn load_from(path: &Path, ttl_secs: u64) -> Self {
        let mut cache = Self {
            ttl_secs,
            peers: BTreeMap::new(),
            dirty: false,
        };
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(file) = toml::from_str::<PeerFile>(&text)
        {
            let now = now_epoch();
            for e in file.peer {
                if e.expires_at <= now {
                    continue; // already expired before we even loaded
                }
                let key = if e.server_id.is_empty() {
                    format!(
                        "{}:{}",
                        e.addrs.first().map(String::as_str).unwrap_or("?"),
                        e.tcp_port
                    )
                } else {
                    e.server_id.clone()
                };
                cache.peers.insert(key, e);
            }
        }
        cache
    }

    pub fn save(&mut self) -> io::Result<()> {
        self.save_to(&peers_path())
    }

    fn save_to(&mut self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = PeerFile {
            peer: self.peers.values().cloned().collect(),
        };
        let text = toml::to_string_pretty(&file)
            .map_err(|e| io::Error::other(format!("peers serialize: {e}")))?;
        fs::write(path, text)?;
        self.dirty = false;
        Ok(())
    }

    /// Persist only when something changed since the last save.
    pub fn save_if_dirty(&mut self) {
        if self.dirty
            && let Err(e) = self.save()
        {
            crate::log::warn(&format!("cannot save peers cache: {e}"));
        }
    }

    /// Upsert a peer from an Announce packet or Register message.
    /// New entries start `announced` and get an immediate verify poll.
    /// A `leaving` announce marks the peer stale right away.
    pub fn upsert_announce(&mut self, ann: &Announcement, source: PeerSource) {
        let now = now_epoch();
        let key = ann.server_id.clone().unwrap_or_else(|| ann.tcp_addr());
        let addr = ann.addr.ip().to_string();
        match self.peers.get_mut(&key) {
            Some(e) => {
                if !e.addrs.contains(&addr) {
                    e.addrs.push(addr);
                }
                e.tcp_port = ann.tcp_port;
                e.name = ann.name.clone();
                e.tls = ann.tls;
                if !ann.fingerprint.is_empty() {
                    e.fingerprint = ann.fingerprint.clone();
                }
                if ann.leaving {
                    e.state = PeerState::Stale;
                } else if e.state == PeerState::Stale {
                    e.state = PeerState::Announced;
                    e.poll_at = now;
                }
                e.last_seen = now;
                e.expires_at = now + self.ttl_secs;
            }
            None => {
                self.peers.insert(
                    key,
                    PeerEntry {
                        server_id: ann.server_id.clone().unwrap_or_default(),
                        name: ann.name.clone(),
                        addrs: vec![addr],
                        tcp_port: ann.tcp_port,
                        tls: ann.tls,
                        fingerprint: ann.fingerprint.clone(),
                        state: if ann.leaving {
                            PeerState::Stale
                        } else {
                            PeerState::Announced
                        },
                        psk_ok: false,
                        sessions: Vec::new(),
                        source,
                        last_seen: now,
                        expires_at: now + self.ttl_secs,
                        poll_at: now, // verify immediately
                    },
                );
            }
        }
        self.dirty = true;
    }

    /// A verification poll succeeded: sessions + fingerprint recorded,
    /// TTL refreshed, next poll scheduled inside `[0.5, 0.9] * ttl`.
    pub fn mark_verified(
        &mut self,
        key: &str,
        sessions: Vec<String>,
        fingerprint: &str,
        psk_ok: bool,
    ) {
        let now = now_epoch();
        if let Some(e) = self.peers.get_mut(key) {
            e.state = PeerState::Verified;
            e.sessions = sessions;
            if !fingerprint.is_empty() {
                e.fingerprint = fingerprint.to_string();
            }
            e.psk_ok = psk_ok;
            e.last_seen = now;
            e.expires_at = now + self.ttl_secs;
            e.poll_at = now + rand_range(self.ttl_secs / 2, self.ttl_secs * 9 / 10);
            self.dirty = true;
        }
    }

    /// The peer stopped answering. Kept in cache (flagged) until TTL expiry.
    pub fn mark_stale(&mut self, key: &str) {
        let now = now_epoch();
        if let Some(e) = self.peers.get_mut(key) {
            e.state = PeerState::Stale;
            // Retry a few times within the TTL window rather than once.
            e.poll_at = now + rand_range(self.ttl_secs / 10, self.ttl_secs / 4);
            self.dirty = true;
        }
    }

    /// Peers whose verification/refresh poll is due. Returns cache keys.
    /// Stale peers are included — they keep retrying within their TTL
    /// window so a temporarily unreachable peer recovers.
    pub fn due_for_poll(&self) -> Vec<String> {
        let now = now_epoch();
        self.peers
            .iter()
            .filter(|(_, e)| e.poll_at <= now)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Drop entries past their TTL. Stale entries re-polled on next load
    /// are handled via poll_at, not here.
    pub fn evict_expired(&mut self) {
        let now = now_epoch();
        let before = self.peers.len();
        self.peers.retain(|_, e| e.expires_at > now);
        self.dirty |= self.peers.len() != before;
    }

    pub fn get(&self, key: &str) -> Option<&PeerEntry> {
        self.peers.get(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = &PeerEntry> {
        self.peers.values()
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// Persisted cache path: `~/.config/lrmux/peers.toml`.
pub fn peers_path() -> PathBuf {
    crate::config::config_dir().join("peers.toml")
}

/// Verification poll budget per peer — a dead peer costs the caller at
/// most this much latency (connect + handshake + query).
const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Poll one peer over TCP: Identify (auth) + ListSessions.
/// Returns `(session names, psk_ok)` on success. `psk_ok` means our
/// configured PSK was accepted — trivially true when we have none and
/// the peer let us in anyway.
///
/// Synchronous and fully bounded: every stage uses connect_timeout or a
/// poll()-based deadline so a dead peer cannot hang the caller.
pub fn poll_peer(addr: &str) -> io::Result<(Vec<String>, bool)> {
    use crate::proto::{ClientMsg, ServerMsg};

    let mut stream = crate::ipc::connect_tcp_timeout(addr, POLL_TIMEOUT)?;
    let psk = crate::config::effective_psk().to_string();
    let ident = crate::proto::encode_client(&ClientMsg::Identify {
        rows: 0,
        cols: 0,
        attach: false,
        auth_token: psk.clone(),
    });
    crate::proto::send(&mut stream, &ident)?;
    let ack = crate::ipc::stream::decode_with_deadline(&mut stream, POLL_TIMEOUT, |r| {
        crate::proto::decode_server(r)
    })?;
    match ack {
        ServerMsg::IdentifyAck { .. } => {}
        ServerMsg::Error { msg } => {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected reply to Identify",
            ));
        }
    }

    let msg = crate::proto::encode_client(&ClientMsg::ListSessions);
    crate::proto::send(&mut stream, &msg)?;
    let res = crate::ipc::stream::decode_with_deadline(&mut stream, POLL_TIMEOUT, |r| {
        crate::proto::decode_server(r)
    })?;
    match res {
        ServerMsg::SessionList { sessions, .. } => {
            let names = sessions.into_iter().map(|s| s.name).collect();
            Ok((names, !psk.is_empty()))
        }
        ServerMsg::Error { msg } => Err(io::Error::new(io::ErrorKind::InvalidData, msg)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected reply to ListSessions",
        )),
    }
}

/// Register this server with a manager (`[peers] managers` entries).
/// Bounded and best-effort: a unreachable manager only delays startup
/// by POLL_TIMEOUT each.
pub fn register(addr: &str, tcp_port: u16, tls: bool) -> io::Result<()> {
    use crate::proto::{ClientMsg, ServerMsg};

    let mut stream = crate::ipc::connect_tcp_timeout(addr, POLL_TIMEOUT)?;
    let ident = crate::proto::encode_client(&ClientMsg::Identify {
        rows: 0,
        cols: 0,
        attach: false,
        auth_token: crate::config::effective_psk().to_string(),
    });
    crate::proto::send(&mut stream, &ident)?;
    let ack = crate::ipc::stream::decode_with_deadline(&mut stream, POLL_TIMEOUT, |r| {
        crate::proto::decode_server(r)
    })?;
    match ack {
        ServerMsg::IdentifyAck { .. } => {}
        ServerMsg::Error { msg } => {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected reply to Identify",
            ));
        }
    }

    let reg = crate::proto::encode_client(&ClientMsg::Register {
        server_id: crate::config::server_id().to_string(),
        name: crate::server::server_name().to_string(),
        tcp_port,
        tls,
        fingerprint: crate::server::tls_fingerprint().to_string(),
    });
    crate::proto::send(&mut stream, &reg)?;
    let res = crate::ipc::stream::decode_with_deadline(&mut stream, POLL_TIMEOUT, |r| {
        crate::proto::decode_server(r)
    })?;
    match res {
        ServerMsg::RegisterAck { ok: true, .. } => Ok(()),
        ServerMsg::RegisterAck { reason, .. } => {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, reason))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected reply to Register",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn ann(id: &str, name: &str) -> Announcement {
        Announcement {
            name: name.to_string(),
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)), 17280),
            tcp_port: 17280,
            tls: true,
            version: "v0".to_string(),
            fingerprint: "ab".to_string(),
            server_id: Some(id.to_string()),
            leaving: false,
        }
    }

    #[test]
    fn upsert_dedups_by_server_id() {
        let mut c = PeerCache::load_from(Path::new("/nonexistent"), DEFAULT_TTL_SECS);
        c.upsert_announce(&ann("id-1", "a"), PeerSource::Broadcast);
        let mut moved = ann("id-1", "a2");
        moved.tcp_port = 9999;
        c.upsert_announce(&moved, PeerSource::Broadcast);
        assert_eq!(c.len(), 1);
        let e = c.get("id-1").unwrap();
        assert_eq!(e.name, "a2");
        assert_eq!(e.tcp_port, 9999);
        assert_eq!(e.state, PeerState::Announced);
    }

    #[test]
    fn verify_then_expire() {
        let mut c = PeerCache::load_from(Path::new("/nonexistent"), DEFAULT_TTL_SECS);
        c.upsert_announce(&ann("id-1", "a"), PeerSource::Scan);
        c.mark_verified("id-1", vec!["main".into()], "cd", true);
        let e = c.get("id-1").unwrap();
        assert_eq!(e.state, PeerState::Verified);
        assert!(e.psk_ok);
        assert_eq!(e.sessions, vec!["main"]);
        // Jittered refresh lands within [0.5, 0.9] * ttl.
        let now = now_epoch();
        assert!(e.poll_at >= now + DEFAULT_TTL_SECS / 2);
        assert!(e.poll_at <= now + DEFAULT_TTL_SECS * 9 / 10);
        // Force expiry.
        c.peers.get_mut("id-1").unwrap().expires_at = now - 1;
        c.evict_expired();
        assert!(c.is_empty());
    }

    #[test]
    fn leaving_marks_stale() {
        let mut c = PeerCache::load_from(Path::new("/nonexistent"), DEFAULT_TTL_SECS);
        c.upsert_announce(&ann("id-1", "a"), PeerSource::Broadcast);
        let mut bye = ann("id-1", "a");
        bye.leaving = true;
        c.upsert_announce(&bye, PeerSource::Broadcast);
        assert_eq!(c.get("id-1").unwrap().state, PeerState::Stale);
    }

    #[test]
    fn save_load_roundtrip_drops_expired() {
        let dir = std::env::temp_dir().join(format!("lrmux-peers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers.toml");

        let mut c = PeerCache::load_from(&path, DEFAULT_TTL_SECS);
        c.upsert_announce(&ann("id-live", "a"), PeerSource::Register);
        c.upsert_announce(&ann("id-dead", "b"), PeerSource::Register);
        c.peers.get_mut("id-dead").unwrap().expires_at = now_epoch() - 1;
        c.save_to(&path).unwrap();

        let c2 = PeerCache::load_from(&path, DEFAULT_TTL_SECS);
        assert_eq!(c2.len(), 1);
        assert!(c2.get("id-live").is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
