//! Application-level Ping/Pong keepalive for client↔server connections.
//!
//! Any received frame resets the idle clock. When idle for `interval`, a Ping
//! is sent; if nothing arrives within `timeout` after that Ping, the peer is
//! considered dead.

use std::time::{Duration, Instant};

/// Action requested by [`Keepalive::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Idle long enough — send a Ping now.
    SendPing,
    /// Ping was sent and no traffic arrived before the timeout.
    TimedOut,
}

/// Per-connection keepalive state.
#[derive(Debug)]
pub struct Keepalive {
    interval: Duration,
    timeout: Duration,
    last_rx: Instant,
    ping_sent_at: Option<Instant>,
}

impl Keepalive {
    /// Build from config + negotiated protocol. Returns `None` when disabled
    /// (`keepalive_secs == 0`) or the peer does not speak keepalive.
    pub fn maybe(interval_secs: u64, timeout_secs: u64, proto: u16) -> Option<Self> {
        if interval_secs == 0 || proto < crate::proto::PROTO_KEEPALIVE {
            return None;
        }
        let timeout_secs = timeout_secs.max(1);
        Some(Self {
            interval: Duration::from_secs(interval_secs),
            timeout: Duration::from_secs(timeout_secs),
            last_rx: Instant::now(),
            ping_sent_at: None,
        })
    }

    /// Any inbound frame (including Pong) counts as liveness.
    pub fn note_rx(&mut self) {
        self.last_rx = Instant::now();
        self.ping_sent_at = None;
    }

    /// Record that we just sent a Ping (starts the timeout clock).
    pub fn note_ping_sent(&mut self) {
        self.ping_sent_at = Some(Instant::now());
    }

    /// Decide whether to ping or declare the peer dead.
    pub fn poll(&self, now: Instant) -> Option<Action> {
        if let Some(sent) = self.ping_sent_at {
            if now.saturating_duration_since(sent) >= self.timeout {
                return Some(Action::TimedOut);
            }
            return None;
        }
        if now.saturating_duration_since(self.last_rx) >= self.interval {
            return Some(Action::SendPing);
        }
        None
    }

    /// Suggested poll timeout so the next ping/timeout fires promptly.
    pub fn poll_timeout_ms(&self, now: Instant) -> i32 {
        let remaining = if let Some(sent) = self.ping_sent_at {
            self.timeout
                .saturating_sub(now.saturating_duration_since(sent))
        } else {
            self.interval
                .saturating_sub(now.saturating_duration_since(self.last_rx))
        };
        remaining.as_millis().clamp(1, i32::MAX as u128) as i32
    }
}

/// Next reconnect delay after `attempt` failures (0-based), capped at 30s.
pub fn reconnect_backoff(attempt: u32) -> Duration {
    let shift = attempt.min(6); // 0.5, 1, 2, 4, 8, 16, 30
    let ms = 500u64.saturating_mul(1u64 << shift).min(30_000);
    Duration::from_millis(ms)
}

/// Human-readable connection target for overlays / diagnostics.
pub fn connection_target(socket_path: &std::path::Path) -> String {
    if let Some(addr) = crate::ipc::tcp_addr() {
        format!("tcp {addr}")
    } else {
        format!("unix {}", socket_path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_interval_zero_or_legacy_proto() {
        assert!(Keepalive::maybe(0, 15, crate::proto::PROTO_KEEPALIVE).is_none());
        assert!(Keepalive::maybe(60, 15, crate::proto::PROTO_LEGACY).is_none());
        assert!(Keepalive::maybe(60, 15, crate::proto::PROTO_KEEPALIVE).is_some());
    }

    #[test]
    fn idle_then_ping_then_timeout() {
        let mut ka = Keepalive::maybe(2, 1, crate::proto::PROTO_KEEPALIVE).unwrap();
        let t0 = ka.last_rx;
        assert_eq!(ka.poll(t0 + Duration::from_millis(500)), None);
        assert_eq!(
            ka.poll(t0 + Duration::from_secs(2)),
            Some(Action::SendPing)
        );
        ka.note_ping_sent();
        // Freeze ping_sent_at by reading it after note.
        let sent = ka.ping_sent_at.unwrap();
        assert_eq!(ka.poll(sent + Duration::from_millis(500)), None);
        assert_eq!(
            ka.poll(sent + Duration::from_secs(1)),
            Some(Action::TimedOut)
        );
        ka.note_rx();
        assert!(ka.ping_sent_at.is_none());
        assert_eq!(ka.poll(ka.last_rx + Duration::from_millis(100)), None);
    }

    #[test]
    fn reconnect_backoff_caps_at_30s() {
        assert_eq!(reconnect_backoff(0), Duration::from_millis(500));
        assert_eq!(reconnect_backoff(1), Duration::from_secs(1));
        assert_eq!(reconnect_backoff(2), Duration::from_secs(2));
        assert_eq!(reconnect_backoff(6), Duration::from_secs(30));
        assert_eq!(reconnect_backoff(20), Duration::from_secs(30));
    }
}
