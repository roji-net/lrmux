// IPC: Unix socket and TCP transport.

pub mod stream;

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

pub use stream::{ConnListener, ConnStream};

/// Compute the socket path for a given server name.
/// Format: /tmp/lrmux-<UID>/<server-name>
pub fn socket_path(server_name: &str) -> PathBuf {
    let uid = unsafe { libc::getuid() };
    let dir = format!("/tmp/lrmux-{uid}");
    Path::new(&dir).join(server_name)
}

/// Bind a Unix socket listener at the given path.
/// Removes any stale socket file first. Creates the parent directory.
pub fn listen(path: &Path) -> io::Result<UnixListener> {
    // Create parent directory with 0700 permissions.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(parent, perms)?;
    }
    // Remove stale socket file if it exists.
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    // Set umask to 077 so the socket file gets 0600 permissions.
    let old_umask = unsafe { libc::umask(0o077) };
    let listener = UnixListener::bind(path);
    unsafe { libc::umask(old_umask) };
    listener
}

/// Connect to a server at the given socket path.
pub fn connect(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path)
}

/// Connect to a server via TCP. Returns a ConnStream.
pub fn connect_tcp(addr: &str) -> io::Result<ConnStream> {
    let stream = std::net::TcpStream::connect(addr)?;
    Ok(ConnStream::Tcp(stream))
}

/// Bind a TCP listener at the given address.
pub fn listen_tcp(addr: &str) -> io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(addr)?;
    Ok(listener)
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
