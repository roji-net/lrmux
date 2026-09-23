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

/// Broadcast a Discover probe and collect Announce replies until timeout.
pub fn discover(port: u16, timeout: Duration) -> io::Result<Vec<Announcement>> {
    let sock = UdpSocket::bind(("0.0.0.0", 0))?;
    sock.set_broadcast(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(50)))?;
    let probe = encode_discover();
    sock.send_to(&probe, (Ipv4Addr::BROADCAST, port))?;
    // Also try subnet-local common broadcast via 255.255.255.255 (already done)
    // and limited broadcast to localhost for same-host servers.
    let _ = sock.send_to(&probe, (Ipv4Addr::LOCALHOST, port));

    let mut found: Vec<Announcement> = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 1500];
    while Instant::now() < deadline {
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
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
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
}
