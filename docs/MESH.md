# lrmux — LAN Discovery & Mesh Design

> Peer discovery, a peer directory ("SessionManager"), and authenticated relay
> for connecting lrmux nodes across a LAN and beyond. Designed to work where
> UDP broadcast and multicast are unavailable (Wi-Fi with client isolation,
> VPNs, emulated or otherwise limited network stacks).

Status: **draft** — wire formats are proposals until implemented.

---

## 1. Motivation

The current discovery model is UDP broadcast: clients send a `Discover`
probe to `255.255.255.255:<port>`, servers reply with a unicast `Announce`.
This works on simple LANs but fails when:

- **Broadcast is not implemented** — some emulated/limited network stacks
  reject `sendto(255.255.255.255)` outright (`EHOSTUNREACH`).
- **Broadcast is blocked** — Wi-Fi AP client isolation, VLANs, VPN links.
- **The peer is off-subnet** — broadcast never leaves the L2 domain.

### 1.1 Capabilities we cannot assume

Measured on constrained/emulated platforms; the design must degrade
gracefully when any of these are missing:

| Capability | May be | Consequence |
|---|---|---|
| `sendto(255.255.255.255)` | `EHOSTUNREACH` | limited broadcast unusable |
| directed broadcast `x.y.z.255` | send accepted, delivery unknown | keep best-effort only |
| `SO_RCVTIMEO` | `EINVAL` | use `poll()` for timeouts — never `set_read_timeout` |
| `getifaddrs` | `EINVAL` | derive subnet via `connect()+getsockname()` or config |
| `SO_REUSEPORT` / 2nd same-port UDP bind | `EINVAL`/`EADDRINUSE` | **one discovery responder per host** |
| `IP_ADD_MEMBERSHIP` | `EINVAL` | no multicast → mDNS/SSDP impossible |
| UDP unicast, TCP connect/listen | reliable | the baseline transport |

Consequences: only unicast UDP and TCP are guaranteed; the local subnet
must be derived via `connect()+getsockname()` or configured explicitly.

---

## 2. Architecture: three roles

```
Server          Hosts sessions (PTYs, grids). Announces/registers itself,
                answers discovery, serves clients. Optionally embeds the
                directory role.

Client          Terminal attach. Bootstraps from local sockets, a configured
                manager, discovery probes, or the persisted peer cache.
                Attaches directly or via relay.

SessionManager  Standalone directory/broker process (`lrmux manager`).
                Holds the peer cache, polls peers, accepts registrations,
                answers ListPeers, optionally relays traffic.
                Hosts no sessions of its own.
```

The directory is a module shared by both deployments:

- `lrmux manager` — standalone, no sessions (e.g. a Home Assistant add-on
  running 24/7 as the well-known rendezvous).
- `[peers] directory = true` on a regular server — a session host that also
  keeps the peer table.

A leaf node's minimal config is one line: the address of a manager. From
there it learns the rest of the network over TCP, the baseline
transport that works everywhere.

---

## 3. Identity & trust

### 3.1 Server identity

- Each server generates a `server_id` (UUID v4) once, persisted at
  `~/.config/lrmux/server.id` (or platform equivalent).
- Peers are keyed by `server_id`, not `ip:port` — TCP ports are
  auto-assigned (`17280+`) and change across restarts.

### 3.2 Trust states

Announce packets are unauthenticated and spoofable. The cache tracks:

```
announced → (TCP poll ok, fingerprint matches) → verified
          → (poll fails)                       → stale → TTL expiry → evict
```

- The TLS fingerprint is pinned at first contact (TOFU). A fingerprint
  change flags the peer (reinstall or MITM); surfaces in `lrmux ls`.
- `Register`, `PeerList` exchange, and `RelayOpen` all require an
  authenticated channel — i.e. the peer must present the same PSK over TLS
  (or plaintext within `safe_networks` under `tls = "auto"`).

### 3.3 Relay trust

`RelayOpen` requires authentication — an open relay would be a LAN-wide
TCP proxy (SSRF). The relay pipes raw bytes; the client performs its own
TLS+PSK handshake *through* the pipe, terminating at the destination
server. The relay node cannot read session traffic.

---

## 4. Wire protocol

### 4.1 UDP packets (port `network.discovery_port`, default 17280)

v1 (current): `LRMX` | ver=1 | type — Discover=1, Announce=2.

**Announce v2** (ver=2) appends:

```
server_id:  16 bytes  UUID
flags:      u8        bit0 = tls, bit1 = leaving (graceful shutdown)
tcp_port:   u16
name:       len-prefixed str
version:    len-prefixed str
fingerprint: len-prefixed str (hex SHA-256 of TLS cert)
```

`leaving` is the "I'm shutting down" notice — receivers mark the peer stale
immediately instead of waiting for TTL.

v1 parsers must ignore v2 packets (unknown version → drop) and vice versa.

### 4.2 TCP protocol additions (existing framed channel)

Client→Server:

- `Register { server_id, name, tcp_addr, tls, fingerprint }` — self-announce
  to a manager. Requires auth.
- `ListPeers` — request the peer table. Requires auth.
- `RelayOpen { addr }` — open a byte pipe to `addr` (a `host:port` string,
  typically another lrmux server). Requires auth.

Server→Client:

- `RegisterAck { ok, reason }`
- `PeerList { peers: [PeerEntry] }` — see §5 for fields.
- `RelayReady` / `RelayError { reason }` — after `RelayReady`, the stream
  carries raw relayed bytes in both directions.

Polling a peer is an ordinary authenticated client session:
`Identify → ListSessions → ListPeers → disconnect`. No separate
server↔server protocol is needed for the first version.

---

## 5. Peer cache

Owned by the directory role (manager or `directory = true` server).
Persisted at `~/.config/lrmux/peers.toml` with **absolute epoch
timestamps** so TTL survives restarts.

```json
{
  "server_id": "…uuid…",
  "name": "work",
  "addrs": ["192.168.1.20"],
  "tcp_port": 17280,
  "tls": true,
  "fingerprint": "sha256:…",
  "state": "verified",            // announced | verified | stale
  "sessions": ["main", "music"],  // last ListSessions snapshot
  "psk_ok": true,                 // our PSK was accepted by this peer
  "source": "register",           // broadcast | scan | register | gossip
  "last_seen": 1760000000,        // epoch secs
  "expires_at": 1760000600
}
```

Lifecycle:

- On announce/register → upsert, state `announced`, schedule verification
  poll.
- Poll succeeds → `verified`, `psk_ok` set, `expires_at = now + ttl`.
- Refresh poll scheduled at a **random point in `[0.5, 0.9] × ttl`** to
  avoid herd synchronization.
- Poll fails → `stale` (kept for display, marked unreachable).
- `now > expires_at` → evict. `leaving` announce → immediate `stale`.
- On load: drop expired entries, re-poll `stale` ones immediately.

---

## 6. Discovery & registration flows

### 6.1 Client bootstrap order

1. Configured managers (`[peers] managers`) — TCP `ListPeers`.
2. Persisted peer cache — direct `ListSessions` to remembered peers.
3. UDP `Discover` to: `255.255.255.255` (best-effort), directed broadcast
   per local subnet, `127.0.0.1`, and `[peers].scan` targets.
4. **Unicast subnet scan** if no replies after a short window: derive
   subnets from `getifaddrs` (where available) or `connect()+getsockname()`
   with an assumed `/24`; send `Discover` to each host. Capped at 1024
   targets; `scan` CIDRs are always scanned.

### 6.2 Server announces

Active announces (not just probe responses):

- On startup and on state changes (new session, shutdown `leaving`).
- Heartbeat every ~60 s.
- Destinations: `255.255.255.255` (best-effort — absent on some platforms), directed
  broadcast, and **unicast to every known peer/manager** — the only
  usable path where broadcast is absent.
- `[network] announce = false` disables active announces.

### 6.3 Registration

For nodes that can't announce (behind NAT, broadcast-less nets):

- Server config: `[peers] managers = ["ha.local:7723"]` → on startup,
  `Register` over TCP to each.
- Manager config: `[peers] accept_registrations = true`.
- **Verification by callback**: the manager polls the announced
  `tcp_addr`; success + matching fingerprint → `verified`. A spoofed
  registration simply never verifies.
- If the leaf can't accept inbound TCP, it stays `announced` (listed,
  flagged). A persistent outbound channel from leaf → manager (reverse
  tunnel) is a possible later phase.

---

## 7. Relay

```
client --[TCP+TLS]--> manager/relay --[TCP]--> target server
         `------------- TLS end-to-end -------------´
```

- `RelayOpen { addr }` on an authenticated connection; relay replies
  `RelayReady` and pipes bytes to/from a fresh TCP connect to `addr`.
- The client then runs the normal Identify/TLS handshake through the pipe;
  the relay sees ciphertext only.
- `[peers] relay = true` enables the role. Relayed connections count in
  the state file for crash forensics.

Use case: off-LAN access (client reaches only the exposed manager), or
reaching a peer behind NAT that registered to the manager.

---

## 8. Configuration

```toml
[network]
discovery       = true      # answer UDP Discover probes (existing)
discovery_port  = 17280
announce        = true      # active announces (broadcast + unicast gossip)
scan            = []        # extra CIDRs/hosts to unicast-probe, e.g.
                            # ["10.17.17.0/24", "192.168.1.5"]

[peers]
directory            = false   # hold peer cache, answer ListPeers
poll                 = true    # verify/refresh peers over TCP
ttl_secs             = 600
managers             = []      # managers to Register with on startup
accept_registrations = false   # accept Register from other nodes
relay                = false   # allow authenticated RelayOpen
```

---

## 9. Phases

- **P0 — transport fixes** (works on every socket stack): ✅ done.
  `discover()` uses `poll()` instead of `SO_RCVTIMEO`; non-fatal
  `set_broadcast`; unicast subnet scan + `scan` config; outbound-IP via
  `connect()+getsockname()`.
- **P1 — directory**: `server.id` ✅, Announce v2 + active announce ✅,
  peer cache + persistence ✅, `Register`/`accept_registrations`/`managers`
  ✅, `ListPeers`/`PeerList` ✅, `lrmux manager` ✅, `list-peers` +
  inventory shows manager-cached peers ✅.
- **P2 — relay**: `RelayOpen` byte pipe, client `attach --via <manager>`
  with end-to-end TLS.
- **P3 — later**: live peer-change notifications to clients, leaf→manager
  persistent channel (reverse tunnel), fingerprint TOFU UI prompts.

## 10. Non-goals (for now)

- mDNS/multicast (unavailable on some platforms; revisit later).
- NAT traversal / hole punching / public rendezvous.
- Multi-hop routing (relay is single-hop).
- Consensus — the cache is eventually-consistent gossip, not replicated state.
