// ConnStream: abstraction over UnixStream, TcpStream, TLS-wrapped TCP, and WebSocket.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use rustls::{ClientConnection, ServerConnection, StreamOwned};

use super::ws::WsByteBridge;

/// A stream that can be Unix, plain TCP, TLS-over-TCP, or WebSocket.
pub enum ConnStream {
    Unix(UnixStream),
    Tcp(TcpStream),
    TlsClient(Box<StreamOwned<ClientConnection, TcpStream>>),
    TlsServer(Box<StreamOwned<ServerConnection, TcpStream>>),
    Ws(Box<WsByteBridge>),
}

impl ConnStream {
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            ConnStream::Unix(s) => s.set_nonblocking(nonblocking),
            ConnStream::Tcp(s) => s.set_nonblocking(nonblocking),
            ConnStream::TlsClient(s) => s.sock.set_nonblocking(nonblocking),
            ConnStream::TlsServer(s) => s.sock.set_nonblocking(nonblocking),
            ConnStream::Ws(s) => s.set_nonblocking(nonblocking),
        }
    }

    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        match self {
            ConnStream::Unix(s) => s.set_read_timeout(dur),
            ConnStream::Tcp(s) => s.set_read_timeout(dur),
            ConnStream::TlsClient(s) => s.sock.set_read_timeout(dur),
            ConnStream::TlsServer(s) => s.sock.set_read_timeout(dur),
            ConnStream::Ws(s) => s.set_read_timeout(dur),
        }
    }

    pub fn as_raw_fd(&self) -> RawFd {
        match self {
            ConnStream::Unix(s) => s.as_raw_fd(),
            ConnStream::Tcp(s) => s.as_raw_fd(),
            ConnStream::TlsClient(s) => s.sock.as_raw_fd(),
            ConnStream::TlsServer(s) => s.sock.as_raw_fd(),
            ConnStream::Ws(s) => s.as_raw_fd(),
        }
    }

    /// True when this connection is not a local Unix socket (TCP / TLS / WS).
    /// Remote transports require PSK when one is configured.
    pub fn is_tcp(&self) -> bool {
        !matches!(self, ConnStream::Unix(_))
    }

    pub fn is_ws(&self) -> bool {
        matches!(self, ConnStream::Ws(_))
    }

    /// TLS and WebSocket are not a raw byte pipe. Reading or writing the
    /// file descriptor directly bypasses the framing and corrupts the session.
    pub fn is_framed(&self) -> bool {
        matches!(
            self,
            ConnStream::TlsClient(_) | ConnStream::TlsServer(_) | ConnStream::Ws(_)
        )
    }

    /// Poll until the socket is readable or `timeout` elapses.
    /// Returns Ok(false) on timeout. poll()-based because SO_RCVTIMEO is
    /// unimplemented on some platforms — unimplemented on some platforms.
    pub fn wait_readable(&self, timeout: std::time::Duration) -> io::Result<bool> {
        wait_io(self.as_raw_fd(), libc::POLLIN, timeout)
    }

    /// Ciphertext (or a WS frame) still queued above the socket.
    /// The poll loop must watch POLLOUT even when the plaintext outbuf is empty.
    pub fn wants_write(&self) -> bool {
        match self {
            ConnStream::TlsClient(s) => s.conn.wants_write(),
            ConnStream::TlsServer(s) => s.conn.wants_write(),
            ConnStream::Ws(_) | ConnStream::Unix(_) | ConnStream::Tcp(_) => false,
        }
    }
}

impl Read for ConnStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ConnStream::Unix(s) => s.read(buf),
            ConnStream::Tcp(s) => s.read(buf),
            ConnStream::TlsClient(s) => s.read(buf),
            ConnStream::TlsServer(s) => s.read(buf),
            ConnStream::Ws(s) => s.read(buf),
        }
    }
}

impl Write for ConnStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ConnStream::Unix(s) => s.write(buf),
            ConnStream::Tcp(s) => s.write(buf),
            ConnStream::TlsClient(s) => s.write(buf),
            ConnStream::TlsServer(s) => s.write(buf),
            ConnStream::Ws(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ConnStream::Unix(s) => s.flush(),
            ConnStream::Tcp(s) => s.flush(),
            ConnStream::TlsClient(s) => s.flush(),
            ConnStream::TlsServer(s) => s.flush(),
            ConnStream::Ws(s) => s.flush(),
        }
    }
}

/// Wait until `fd` reports `events` (POLLIN, POLLIN|POLLOUT, …) or
/// `timeout` elapses. Returns Ok(false) on timeout.
pub fn wait_io(fd: RawFd, events: i16, timeout: std::time::Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events,
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
    Ok(r > 0 && pfd.revents & events != 0)
}

/// Read adapter that bounds a decode by an absolute deadline: polls the fd
/// before retrying WouldBlock reads, so a silent or stalling peer cannot
/// hang the caller. Portable replacement for SO_RCVTIMEO — it is unimplemented on some platforms.
/// Expects the wrapped stream in nonblocking mode.
pub struct DeadlineReader<'a> {
    inner: &'a mut ConnStream,
    deadline: std::time::Instant,
}

impl<'a> DeadlineReader<'a> {
    pub fn new(inner: &'a mut ConnStream, timeout: std::time::Duration) -> Self {
        Self {
            inner,
            deadline: std::time::Instant::now() + timeout,
        }
    }
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let now = std::time::Instant::now();
            if now >= self.deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "read deadline exceeded",
                ));
            }
            match self.inner.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !self.inner.wait_readable(self.deadline - now)? {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "read deadline exceeded",
                        ));
                    }
                }
                other => return other,
            }
        }
    }
}

/// Run `decode` against `stream` with all reads bounded by `timeout`.
/// Temporarily switches the socket to nonblocking mode; restores blocking
/// mode before returning so subsequent writes are unaffected.
pub fn decode_with_deadline<T>(
    stream: &mut ConnStream,
    timeout: std::time::Duration,
    decode: impl FnOnce(&mut dyn Read) -> io::Result<T>,
) -> io::Result<T> {
    stream.set_nonblocking(true)?;
    let mut rd = DeadlineReader::new(stream, timeout);
    let res = decode(&mut rd);
    let _ = stream.set_nonblocking(false);
    res
}

/// A listener that can accept Unix, TCP, or WebSocket connections.
pub enum ConnListener {
    Unix(std::os::unix::net::UnixListener),
    Tcp(std::net::TcpListener),
    /// Plain TCP listener that performs a WebSocket handshake on accept.
    Ws(std::net::TcpListener),
}

impl ConnListener {
    pub fn as_raw_fd(&self) -> RawFd {
        match self {
            ConnListener::Unix(l) => l.as_raw_fd(),
            ConnListener::Tcp(l) | ConnListener::Ws(l) => l.as_raw_fd(),
        }
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            ConnListener::Unix(l) => l.set_nonblocking(nonblocking),
            ConnListener::Tcp(l) | ConnListener::Ws(l) => l.set_nonblocking(nonblocking),
        }
    }

    pub fn accept(&self) -> io::Result<ConnStream> {
        match self {
            ConnListener::Unix(l) => {
                let (stream, _) = l.accept()?;
                Ok(ConnStream::Unix(stream))
            }
            ConnListener::Tcp(l) => {
                let (stream, _) = l.accept()?;
                Ok(ConnStream::Tcp(stream))
            }
            ConnListener::Ws(l) => {
                let (stream, _) = l.accept()?;
                // Bound the wait for the first bytes so a connect-and-stall
                // client cannot freeze the event loop. poll()-based:
                // SO_RCVTIMEO is unimplemented on some platforms.
                if !wait_io(
                    stream.as_raw_fd(),
                    libc::POLLIN,
                    std::time::Duration::from_secs(5),
                )? {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "websocket handshake timed out",
                    ));
                }
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
                let bridge = WsByteBridge::accept(stream)?;
                Ok(ConnStream::Ws(Box::new(bridge)))
            }
        }
    }

    pub fn is_tcp(&self) -> bool {
        matches!(self, ConnListener::Tcp(_))
    }

    pub fn is_ws(&self) -> bool {
        matches!(self, ConnListener::Ws(_))
    }
}
