// WebSocket byte bridge: expose tungstenite as a Read/Write stream so the
// existing length-prefixed proto works unchanged over WS binary frames.
//
// Each write() becomes one Binary WebSocket message. Reads concatenate
// incoming Binary/Text payloads into a stream buffer.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};

use tungstenite::{Message, WebSocket, accept};

/// Stream adapter used by `ConnStream::Ws`.
pub struct WsByteBridge {
    ws: WebSocket<TcpStream>,
    read_buf: VecDeque<u8>,
}

impl WsByteBridge {
    /// Complete a server-side WebSocket handshake on an accepted TCP stream.
    pub fn accept(stream: TcpStream) -> io::Result<Self> {
        let ws = accept(stream).map_err(|e| io::Error::other(format!("websocket accept: {e}")))?;
        Ok(Self {
            ws,
            read_buf: VecDeque::new(),
        })
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.ws.get_ref().set_nonblocking(nonblocking)
    }

    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        self.ws.get_ref().set_read_timeout(dur)
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.ws.get_ref().as_raw_fd()
    }

    fn fill_read_buf(&mut self) -> io::Result<()> {
        loop {
            if !self.read_buf.is_empty() {
                return Ok(());
            }
            match self.ws.read() {
                Ok(Message::Binary(data)) => {
                    self.read_buf.extend(data);
                    return Ok(());
                }
                Ok(Message::Text(text)) => {
                    self.read_buf.extend(text.as_bytes());
                    return Ok(());
                }
                Ok(Message::Ping(payload)) => {
                    let _ = self.ws.send(Message::Pong(payload));
                }
                Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
                Ok(Message::Close(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "websocket closed",
                    ));
                }
                Err(tungstenite::Error::Io(e))
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    return Err(e);
                }
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "websocket closed",
                    ));
                }
                Err(e) => return Err(io::Error::other(format!("websocket read: {e}"))),
            }
        }
    }
}

impl Read for WsByteBridge {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.read_buf.is_empty() {
            self.fill_read_buf()?;
        }
        let n = buf.len().min(self.read_buf.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.read_buf.pop_front().unwrap();
        }
        Ok(n)
    }
}

impl Write for WsByteBridge {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // One WS binary frame per write — proto::send writes a full frame then flushes.
        match self.ws.send(Message::Binary(buf.to_vec().into())) {
            Ok(()) => Ok(buf.len()),
            Err(tungstenite::Error::Io(e))
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                Err(e)
            }
            Err(e) => Err(io::Error::other(format!("websocket write: {e}"))),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ws
            .flush()
            .map_err(|e| io::Error::other(format!("websocket flush: {e}")))
    }
}
