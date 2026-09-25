// UDP discovery: clients broadcast Discover; servers unicast Announce.
//
// Disabled unless [network] discovery = true. Default port 17280.
//
// Packet layout (all multi-byte integers little-endian):
//   magic:    4 bytes  "LRMX"
//   version:  u8       1
//   type:     u8       Discover=1, Announce=2
//   -- Announce only --
//   flags:    u8       bit0 = tls
//   tcp_port: u16
//   name_len: u8
//   name:     bytes
//   ver_len:  u8
//   version:  bytes
//   fp_len:   u8
//   fingerprint: bytes (hex SHA-256 of TLS cert, optional)

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 17280;
pub const MAGIC: &[u8; 4] = b"LRMX";
pub const PROTO_VERSION: u8 = 1;
pub const TYPE_DISCOVER: u8 = 1;
pub const TYPE_ANNOUNCE: u8 = 2;
pub const FLAG_TLS: u8 = 0x01;

/// A discovered remote server.
#[derive(Debug, Clone)]
pub struct Announcement {
    pub name: String,
    pub addr: SocketAddr,
    pub tcp_port: u16,
    pub tls: bool,
    pub version: String,
    pub fingerprint: String,
}

impl Announcement {
    /// TCP connect string (ip:port), using the source IP from the UDP reply.
    pub fn tcp_addr(&self) -> String {
        format!("{}:{}", self.addr.ip(), self.tcp_port)
    }
}

/// Encode a Discover probe.
pub fn encode_discover() -> Vec<u8> {
    let mut buf = Vec::with_capacity(6);
    buf.extend_from_slice(MAGIC);
    buf.push(PROTO_VERSION);
    buf.push(TYPE_DISCOVER);
    buf
}

/// Encode an Announce reply.
pub fn encode_announce(
    name: &str,
    tcp_port: u16,
    tls: bool,
    version: &str,
    fingerprint: &str,
) -> Vec<u8> {
    let name_b = name.as_bytes();
    let ver_b = version.as_bytes();
    let fp_b = fingerprint.as_bytes();
    let name_len = name_b.len().min(255) as u8;
    let ver_len = ver_b.len().min(255) as u8;
    let fp_len = fp_b.len().min(255) as u8;
    let mut buf = Vec::with_capacity(
        6 + 1 + 2 + 1 + name_len as usize + 1 + ver_len as usize + 1 + fp_len as usize,
    );
    buf.extend_from_slice(MAGIC);
    buf.push(PROTO_VERSION);
    buf.push(TYPE_ANNOUNCE);
    buf.push(if tls { FLAG_TLS } else { 0 });
    buf.extend_from_slice(&tcp_port.to_le_bytes());
    buf.push(name_len);
    buf.extend_from_slice(&name_b[..name_len as usize]);
    buf.push(ver_len);
    buf.extend_from_slice(&ver_b[..ver_len as usize]);
    buf.push(fp_len);
    buf.extend_from_slice(&fp_b[..fp_len as usize]);
    buf
}

/// Parse a discovery packet. Returns None if not a valid lrmux packet.
pub fn parse_packet(data: &[u8], from: SocketAddr) -> Option<ParsedPacket> {
    if data.len() < 6 || &data[0..4] != MAGIC || data[4] != PROTO_VERSION {
        return None;
    }
    match data[5] {
        TYPE_DISCOVER => Some(ParsedPacket::Discover),
        TYPE_ANNOUNCE => {
            if data.len() < 10 {
                return None;
            }
            let flags = data[6];
            let tcp_port = u16::from_le_bytes([data[7], data[8]]);
            let mut pos = 9;
            let name = read_len_str(data, &mut pos)?;
            let version = read_len_str(data, &mut pos)?;
            let fingerprint = read_len_str(data, &mut pos).unwrap_or_default();
            Some(ParsedPacket::Announce(Announcement {
                name,
                addr: from,
                tcp_port,
                tls: flags & FLAG_TLS != 0,
                version,
                fingerprint,
            }))
        }
        _ => None,
    }
}

fn read_len_str(data: &[u8], pos: &mut usize) -> Option<String> {
    if *pos >= data.len() {
        return None;
    }
    let len = data[*pos] as usize;
    *pos += 1;
    if *pos + len > data.len() {
        return None;
    }
    let s = String::from_utf8_lossy(&data[*pos..*pos + len]).into_owned();
    *pos += len;
    Some(s)
}

pub enum ParsedPacket {
    Discover,
    Announce(Announcement),
}

/// Bind a UDP socket for discovery replies (server side).
///
/// Several servers on one host share the discovery port. `SO_REUSEADDR`
/// (and `SO_REUSEPORT` on BSD/macOS) is set before bind so the second
/// process is not refused. Broadcast probes are delivered to every socket.
pub fn bind_server(port: u16) -> io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;

    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let yes: libc::c_int = 1;
    let yes_len = std::mem::size_of_val(&yes) as libc::socklen_t;
    // Safety: `fd` is a freshly created datagram socket. On failure it is closed.
    let set = |opt| unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            &yes as *const _ as *const libc::c_void,
            yes_len,
        )
    };
    if set(libc::SO_REUSEADDR) < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    // macOS/BSD refuse a second live bind of the same UDP port without
    // SO_REUSEPORT, and deliver broadcast probes to every such socket.
    // On Linux that option load-balances, so a broadcast would reach only
    // one server; SO_REUSEADDR already fans broadcasts out there.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    if set(libc::SO_REUSEPORT) < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = port.to_be();
    addr.sin_addr = libc::in_addr {
        s_addr: u32::from(Ipv4Addr::UNSPECIFIED).to_be(),
    };
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        addr.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }
    let bound = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if bound < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    // Safety: `fd` is a bound datagram socket and is not used again after this move.
    let sock = unsafe { UdpSocket::from_raw_fd(fd) };
    sock.set_nonblocking(true)?;
    sock.set_broadcast(true)?;
    Ok(sock)
}

/// A scan target from config: a single host or a whole subnet to probe.
#[derive(Debug, Clone, Copy)]
enum ScanTarget {
    Host(Ipv4Addr, u16),
    /// IPv4 subnet as (network addr, netmask), host byte order.
    Subnet {
        network: u32,
        mask: u32,
    },
}

/// Parse a `network.scan` entry: "a.b.c.d", "a.b.c.d:port", or "a.b.c.d/n".
fn parse_scan_target(s: &str, default_port: u16) -> Option<ScanTarget> {
    let s = s.trim();
    if let Some((addr, prefix)) = s.split_once('/') {
        let ip: Ipv4Addr = addr.trim().parse().ok()?;
        let bits: u8 = prefix.trim().parse().ok()?;
        if bits > 32 {
            return None;
        }
        let mask = if bits == 0 {
            0
        } else {
            u32::MAX << (32 - bits)
        };
        return Some(ScanTarget::Subnet {
            network: u32::from(ip) & mask,
            mask,
        });
    }
    if let Some((host, p)) = s.rsplit_once(':')
        && let (Ok(ip), Ok(port)) = (host.parse::<Ipv4Addr>(), p.parse::<u16>())
    {
        return Some(ScanTarget::Host(ip, port));
    }
    s.parse::<Ipv4Addr>()
        .ok()
        .map(|ip| ScanTarget::Host(ip, default_port))
}

/// Outbound IPv4 address: the source IP a packet to the internet would use.
/// UDP connect sends no packets — it just picks the route/interface.
/// Works on platforms where getifaddrs/netlink is unavailable.
fn outbound_ipv4() -> Option<u32> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(8, 8, 8, 8), 53)).ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(u32::from(ip)),
        _ => None,
    }
}

/// Local IPv4 subnets as (network, mask) host-order pairs.
/// Prefers getifaddrs (real netmasks); falls back to the outbound IP with
/// an assumed /24 — the common case on LANs and the only option where broadcast is absent.
fn local_ipv4_subnets() -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) == 0 {
            let mut ifa = ifap;
            while !ifa.is_null() {
                let ifa_ref = &*ifa;
                ifa = ifa_ref.ifa_next;
                if ifa_ref.ifa_addr.is_null() || ifa_ref.ifa_netmask.is_null() {
                    continue;
                }
                if (*ifa_ref.ifa_addr).sa_family != libc::AF_INET as libc::sa_family_t {
                    continue;
                }
                let a = &*(ifa_ref.ifa_addr as *const libc::sockaddr_in);
                let m = &*(ifa_ref.ifa_netmask as *const libc::sockaddr_in);
                let ip = u32::from_be(a.sin_addr.s_addr);
                let mask = u32::from_be(m.sin_addr.s_addr);
                // Skip loopback and link-local.
                if ip >> 24 == 127 || (ip >> 16) == 0xA9FE {
                    continue;
                }
                out.push((ip & mask, mask));
            }
            libc::freeifaddrs(ifap);
        }
    }
    if out.is_empty()
        && let Some(ip) = outbound_ipv4()
        && ip >> 24 != 127
    {
        out.push((ip & 0xFFFF_FF00, 0xFFFF_FF00));
    }
    out
}

/// Cap on unicast probes per subnet, so a huge configured CIDR doesn't
/// turn into a flood.
const MAX_SCAN_HOSTS: u32 = 1024;

/// Wait for the socket to be readable; returns false on timeout/interrupt.
fn poll_readable(fd: std::os::fd::RawFd, timeout: Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
    let r = unsafe { libc::poll(&mut pfd, 1, ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(e);
    }
    Ok(r > 0 && pfd.revents & libc::POLLIN != 0)
}

/// Probe for LAN servers and collect Announce replies until timeout.
///
/// Targets, all best-effort (send errors are ignored — e.g. platforms with no
/// broadcast at all):
///   1. 255.255.255.255 limited broadcast + localhost
///   2. Directed broadcast (net.255) per local subnet
///   3. `scan` entries — explicit hosts and extra CIDRs from config
///   4. If nothing answered early on: unicast probe to every host in each
///      local/configured subnet (the only option where broadcast is absent, where broadcast
///      send fails; also defeats AP client isolation)
pub fn discover(port: u16, timeout: Duration, scan: &[String]) -> io::Result<Vec<Announcement>> {
    use std::os::fd::AsRawFd;

    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    // Both best-effort: missing on some platforms (SO_BROADCAST can't fail
    // the whole probe; SO_RCVTIMEO is unusable on some platforms — we poll() instead).
    let _ = sock.set_broadcast(true);
    sock.set_nonblocking(true)?;

    let probe = encode_discover();
    let mut phase1: Vec<SocketAddr> = vec![
        SocketAddr::new(Ipv4Addr::BROADCAST.into(), port),
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
    ];
    let mut subnets = local_ipv4_subnets();
    for t in scan.iter().filter_map(|s| parse_scan_target(s, port)) {
        match t {
            ScanTarget::Host(ip, p) => phase1.push(SocketAddr::new(ip.into(), p)),
            ScanTarget::Subnet { network, mask } => subnets.push((network, mask)),
        }
    }
    subnets.sort_unstable();
    subnets.dedup();

    // Directed broadcast per subnet (phase 1) + unicast host list (phase 2).
    let mut phase2: Vec<SocketAddr> = Vec::new();
    let mut own_ips: std::collections::HashSet<u32> = std::collections::HashSet::new();
    if let Some(ip) = outbound_ipv4() {
        own_ips.insert(ip);
    }
    for &(net, mask) in &subnets {
        let size = (!mask).wrapping_add(1);
        if size < 4 {
            continue; // /31, /32: no usable host range
        }
        phase1.push(SocketAddr::new(Ipv4Addr::from(net | !mask).into(), port));
        for host in (net + 1)..(net + (size - 1).min(MAX_SCAN_HOSTS + 1)) {
            if !own_ips.contains(&host) {
                phase2.push(SocketAddr::new(Ipv4Addr::from(host).into(), port));
            }
        }
    }
    phase2.sort_unstable();
    phase2.dedup();

    for t in &phase1 {
        let _ = sock.send_to(&probe, t);
    }

    // Phase 2 fires only if nothing answered — keeps quiet LANs quiet.
    let scan_at = Instant::now() + timeout.min(Duration::from_millis(300)) / 3;
    let mut scanned = phase2.is_empty();
    let deadline = Instant::now() + timeout;
    let mut found: Vec<Announcement> = Vec::new();
    let mut buf = [0u8; 1500];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if !scanned && now >= scan_at {
            scanned = true;
            if found.is_empty() {
                for t in &phase2 {
                    let _ = sock.send_to(&probe, t);
                }
            }
        }
        let wait = (deadline - now).min(Duration::from_millis(100));
        match poll_readable(sock.as_raw_fd(), wait) {
            Ok(true) => loop {
                match sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if let Some(ParsedPacket::Announce(a)) = parse_packet(&buf[..n], from)
                            && !found
                                .iter()
                                .any(|x| x.name == a.name && x.tcp_addr() == a.tcp_addr())
                        {
                            found.push(a);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            },
            Ok(false) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_discovery_sockets_can_bind_the_same_port() {
        let first = bind_server(0).expect("bind ephemeral");
        let port = first.local_addr().expect("local addr").port();
        let second = bind_server(port);
        assert!(
            second.is_ok(),
            "second discovery bind on {port} failed: {second:?}"
        );
    }

    #[test]
    fn discover_collects_announce_reply() {
        // Fake responder: a bound discovery socket that replies Announce
        // to any packet it gets.
        let responder = bind_server(0).expect("bind responder");
        let port = responder.local_addr().unwrap().port();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            use std::os::fd::AsRawFd;
            let reply = encode_announce("testsrv", 9999, false, "v0", "ff00");
            let mut buf = [0u8; 256];
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                // bind_server returns a nonblocking socket; poll then drain.
                match poll_readable(responder.as_raw_fd(), Duration::from_millis(100)) {
                    Ok(true) => {
                        while let Ok((n, from)) = responder.recv_from(&mut buf) {
                            if matches!(parse_packet(&buf[..n], from), Some(ParsedPacket::Discover))
                            {
                                let _ = responder.send_to(&reply, from);
                            }
                        }
                    }
                    _ => continue,
                }
            }
        });
        let found = discover(port, Duration::from_secs(2), &[]).expect("discover");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap();
        assert!(
            found
                .iter()
                .any(|a| a.name == "testsrv" && a.tcp_port == 9999),
            "expected announce from testsrv, got {found:?}"
        );
    }

    #[test]
    fn parse_scan_target_variants() {
        assert!(matches!(
            parse_scan_target("10.0.0.5", 100).unwrap(),
            ScanTarget::Host(ip, 100) if ip == Ipv4Addr::new(10,0,0,5)
        ));
        assert!(matches!(
            parse_scan_target("10.0.0.5:999", 100).unwrap(),
            ScanTarget::Host(ip, 999) if ip == Ipv4Addr::new(10,0,0,5)
        ));
        match parse_scan_target("192.168.1.7/24", 100).unwrap() {
            ScanTarget::Subnet { network, mask } => {
                assert_eq!(network, u32::from(Ipv4Addr::new(192, 168, 1, 0)));
                assert_eq!(mask, 0xFFFF_FF00);
            }
            _ => panic!("expected subnet"),
        }
        assert!(parse_scan_target("nope", 100).is_none());
        assert!(parse_scan_target("10.0.0.1/33", 100).is_none());
    }
}
