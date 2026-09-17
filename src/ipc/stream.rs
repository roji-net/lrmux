// ConnStream: abstraction over UnixStream and TcpStream.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

/// A stream that can be either a Unix socket or a TCP connection.
/// Both implement Read + Write + AsRawFd, so we can use the same protocol
/// over either transport.
pub enum ConnStream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl ConnStream {
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            ConnStream::Unix(s) => s.set_nonblocking(nonblocking),
            ConnStream::Tcp(s) => s.set_nonblocking(nonblocking),
        }
    }

    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        match self {
            ConnStream::Unix(s) => s.set_read_timeout(dur),
            ConnStream::Tcp(s) => s.set_read_timeout(dur),
        }
    }

    pub fn as_raw_fd(&self) -> RawFd {
        match self {
            ConnStream::Unix(s) => s.as_raw_fd(),
            ConnStream::Tcp(s) => s.as_raw_fd(),
        }
    }
}

impl Read for ConnStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ConnStream::Unix(s) => s.read(buf),
            ConnStream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for ConnStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ConnStream::Unix(s) => s.write(buf),
            ConnStream::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ConnStream::Unix(s) => s.flush(),
            ConnStream::Tcp(s) => s.flush(),
        }
    }
}

/// A listener that can accept either Unix or TCP connections.
pub enum ConnListener {
    Unix(std::os::unix::net::UnixListener),
    Tcp(std::net::TcpListener),
}

impl ConnListener {
    pub fn as_raw_fd(&self) -> RawFd {
        match self {
            ConnListener::Unix(l) => l.as_raw_fd(),
            ConnListener::Tcp(l) => l.as_raw_fd(),
        }
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            ConnListener::Unix(l) => l.set_nonblocking(nonblocking),
            ConnListener::Tcp(l) => l.set_nonblocking(nonblocking),
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
        }
    }
}
