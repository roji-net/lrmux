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
                // Handshake is blocking; bound it so a stuck client cannot stall the loop.
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
