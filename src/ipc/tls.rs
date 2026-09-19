// TLS helpers for remote TCP: rustls + self-signed cert bootstrap.
//
// Policy (TlsMode::Auto): require TLS unless the peer IP is listed in
// `network.safe_networks` (CIDR). Empty safe_networks (default) ⇒ always TLS.
// TlsMode::On always; TlsMode::Off never.

use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection, StreamOwned};

use crate::config::{NetworkConfig, TlsMode, certs_dir};

/// A parsed IPv4/IPv6 CIDR network.
#[derive(Debug, Clone)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (addr_s, pref_s) = s.split_once('/')?;
        let prefix: u8 = pref_s.parse().ok()?;
        let addr: IpAddr = addr_s.parse().ok()?;
        match addr {
            IpAddr::V4(_) if prefix > 32 => return None,
            IpAddr::V6(_) if prefix > 128 => return None,
            _ => {}
        }
        Some(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 {
                    0u32
                } else {
                    u32::MAX << (32 - self.prefix as u32)
                };
                (u32::from(net) & mask) == (u32::from(ip) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let net_b = u128::from(net);
                let ip_b = u128::from(ip);
                let mask = if self.prefix == 0 {
                    0u128
                } else {
                    u128::MAX << (128 - self.prefix as u32)
                };
                (net_b & mask) == (ip_b & mask)
            }
            _ => false,
        }
    }
}

/// Parse `safe_networks` strings; invalid entries are skipped with a warning.
pub fn parse_safe_networks(cidrs: &[String]) -> Vec<Cidr> {
    let mut out = Vec::new();
    for s in cidrs {
        match Cidr::parse(s) {
            Some(c) => out.push(c),
            None => eprintln!("lrmux: warning: invalid safe_networks CIDR '{s}', ignoring"),
        }
    }
    out
}

/// True if `ip` matches any entry in `safe`.
pub fn ip_in_safe_networks(ip: IpAddr, safe: &[Cidr]) -> bool {
    safe.iter().any(|c| c.contains(ip))
}

/// Whether TLS is required for a connection to `peer` under `mode` / `safe_networks`.
pub fn tls_required_for_peer(mode: TlsMode, peer: SocketAddr, safe_networks: &[Cidr]) -> bool {
    match mode {
        TlsMode::Off => false,
        TlsMode::On => true,
        TlsMode::Auto => !ip_in_safe_networks(peer.ip(), safe_networks),
    }
}

/// Convenience: evaluate policy from a full NetworkConfig.
pub fn tls_required(net: &NetworkConfig, peer: SocketAddr) -> bool {
    let safe = parse_safe_networks(&net.safe_networks);
    tls_required_for_peer(net.tls, peer, &safe)
}

/// Resolve default cert/key paths, generating a self-signed pair if missing.
pub fn ensure_server_certs(
    cert_override: &str,
    key_override: &str,
) -> io::Result<(PathBuf, PathBuf)> {
    let cert_path = if cert_override.is_empty() {
        certs_dir().join("server.crt")
    } else {
        PathBuf::from(cert_override)
    };
    let key_path = if key_override.is_empty() {
        certs_dir().join("server.key")
    } else {
        PathBuf::from(key_override)
    };
    if !cert_path.exists() || !key_path.exists() {
        generate_self_signed(&cert_path, &key_path)?;
    }
    Ok((cert_path, key_path))
}

fn generate_self_signed(cert_path: &Path, key_path: &Path) -> io::Result<()> {
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let certified = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])
    .map_err(|e| io::Error::other(format!("cert generation failed: {e}")))?;
    fs::write(cert_path, certified.cert.pem().as_bytes())?;
    fs::write(key_path, certified.key_pair.serialize_pem().as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(key_path, fs::Permissions::from_mode(0o600));
        let _ = fs::set_permissions(cert_path, fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

/// Load PEM cert + key into a rustls ServerConfig.
pub fn load_server_config(cert_path: &Path, key_path: &Path) -> io::Result<Arc<ServerConfig>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert_pem = fs::read(cert_path)?;
    let key_pem = fs::read(key_path)?;
    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::other(format!("parse cert: {e}")))?;
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|e| io::Error::other(format!("parse key: {e}")))?
        .ok_or_else(|| io::Error::other("no private key in PEM"))?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::other(format!("tls server config: {e}")))?;
    Ok(Arc::new(config))
}

/// Client config that accepts any certificate (PSK is the access gate;
/// discovery fingerprint is informational).
pub fn insecure_client_config() -> Arc<ClientConfig> {
    #[derive(Debug)]
    struct NoVerifier;
    impl rustls::client::danger::ServerCertVerifier for NoVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();
    Arc::new(config)
}

/// SHA-256 hex fingerprint of the first cert in a PEM file (for Announce).
pub fn cert_fingerprint_hex(cert_path: &Path) -> io::Result<String> {
    let pem = fs::read(cert_path)?;
    let certs = rustls_pemfile::certs(&mut &pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::other(format!("parse cert: {e}")))?;
    let Some(cert) = certs.first() else {
        return Ok(String::new());
    };
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(cert.as_ref());
    Ok(hash.iter().map(|b| format!("{b:02x}")).collect())
}

/// Perform a TLS server handshake on an accepted TCP stream.
pub fn wrap_server(
    mut tcp: TcpStream,
    config: Arc<ServerConfig>,
) -> io::Result<StreamOwned<ServerConnection, TcpStream>> {
    let mut conn = ServerConnection::new(config)
        .map_err(|e| io::Error::other(format!("tls server conn: {e}")))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)
            .map_err(|e| io::Error::other(format!("tls handshake: {e}")))?;
    }
    Ok(StreamOwned::new(conn, tcp))
}

/// Perform a TLS client handshake to `addr`.
pub fn wrap_client(
    mut tcp: TcpStream,
    server_name: &str,
) -> io::Result<StreamOwned<ClientConnection, TcpStream>> {
    let config = insecure_client_config();
    let name = ServerName::try_from(server_name.to_string())
        .map_err(|e| io::Error::other(format!("invalid server name: {e}")))?;
    let mut conn = ClientConnection::new(config, name)
        .map_err(|e| io::Error::other(format!("tls client conn: {e}")))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)
            .map_err(|e| io::Error::other(format!("tls handshake: {e}")))?;
    }
    Ok(StreamOwned::new(conn, tcp))
}

/// Parse host from "host:port" for ServerName.
pub fn host_from_addr(addr: &str) -> String {
    if let Some(host) = addr.strip_prefix('[')
        && let Some(end) = host.find(']')
    {
        return host[..end].to_string();
    }
    addr.rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or_else(|| addr.to_string())
}
