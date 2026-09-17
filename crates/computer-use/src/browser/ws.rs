//! Minimal RFC 6455 WebSocket client for a loopback DevTools endpoint.
//!
//! Chrome's remote-debugging endpoint is `ws://127.0.0.1:<port>/…`, plain
//! TCP on the loopback interface, text frames only. That is the whole
//! surface this needs: an HTTP/1.1 upgrade, masked client text frames, and
//! unmasked server text/continuation/ping/close frames. No TLS, no
//! extensions, no compression — and no dependency: the workspace keeps its
//! dependency profile deliberately small, and the ~250 lines here are the
//! part of the protocol a loopback control channel actually uses.
//!
//! Every read is bounded by the socket read timeout the caller sets, so a
//! browser that stops answering surfaces as a typed timeout, not a hang.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Largest frame accepted from the server: a `Page.captureScreenshot` of a
/// 1920×1080 page is under a few MiB as base64; DOM/AX trees of ordinary
/// pages are far smaller. Anything larger is refused rather than buffered.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Typed WebSocket failure. Display never echoes payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WsError {
    Connect,
    Handshake,
    Io,
    Timeout,
    Closed,
    FrameBound,
    Protocol,
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connect => "websocket connect failed",
            Self::Handshake => "websocket upgrade handshake failed",
            Self::Io => "websocket I/O error",
            Self::Timeout => "websocket read timed out",
            Self::Closed => "websocket closed by peer",
            Self::FrameBound => "websocket frame exceeds bound",
            Self::Protocol => "websocket protocol violation",
        })
    }
}

impl std::error::Error for WsError {}

/// `(fin, opcode, payload, mask)` of one frame as read off the wire.
type FrameBody = (bool, u8, Vec<u8>, Option<[u8; 4]>);

/// One client connection. Not shareable across threads; the owner serialises
/// request/response pairs on top of it.
pub struct WebSocket {
    stream: TcpStream,
    fragments: Vec<u8>,
    mask_seed: u32,
    /// Set once the byte stream can no longer be trusted: the peer closed,
    /// a frame exceeded the bound (its payload is still on the wire), or a
    /// read/write failed part-way through a frame. Every later call fails
    /// with `Closed`; the owner replaces the connection.
    dead: bool,
}

impl std::fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocket")
            .field("peer", &self.stream.peer_addr().ok())
            .finish_non_exhaustive()
    }
}

impl WebSocket {
    /// Connect to `ws://host:port/path`. Only `ws://` is supported.
    pub fn connect(url: &str, timeout: Duration) -> Result<Self, WsError> {
        let rest = url.strip_prefix("ws://").ok_or(WsError::Connect)?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let addr = authority
            .to_socket_addrs()
            .map_err(|_| WsError::Connect)?
            .next()
            .ok_or(WsError::Connect)?;
        let stream = TcpStream::connect_timeout(&addr, timeout).map_err(|_| WsError::Connect)?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| WsError::Io)?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|_| WsError::Io)?;
        stream.set_nodelay(true).map_err(|_| WsError::Io)?;
        let mut socket = Self {
            stream,
            fragments: Vec::new(),
            mask_seed: seed(),
            dead: false,
        };
        socket.handshake(authority, path)?;
        Ok(socket)
    }

    /// Bound every subsequent read by `timeout`.
    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), WsError> {
        self.stream
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
            .map_err(|_| WsError::Io)
    }

    fn handshake(&mut self, authority: &str, path: &str) -> Result<(), WsError> {
        let key = base64_encode(&self.nonce16());
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\n\
Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        self.stream
            .write_all(request.as_bytes())
            .map_err(|_| WsError::Handshake)?;
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            if response.len() > 16 * 1024 {
                return Err(WsError::Handshake);
            }
            match self.stream.read(&mut byte) {
                Ok(1) => response.push(byte[0]),
                Ok(_) => return Err(WsError::Handshake),
                Err(_) => return Err(WsError::Handshake),
            }
        }
        let text = String::from_utf8_lossy(&response);
        let status_line = text.lines().next().unwrap_or("");
        if !status_line.starts_with("HTTP/1.1 101") {
            return Err(WsError::Handshake);
        }
        let upgraded = text
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("upgrade: websocket"));
        if !upgraded {
            return Err(WsError::Handshake);
        }
        Ok(())
    }

    /// Whether this connection is beyond use (see the `dead` field).
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Send one text frame.
    pub fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        if self.dead {
            return Err(WsError::Closed);
        }
        self.send_frame(0x1, text.as_bytes())
            .inspect_err(|_| self.dead = true)
    }

    /// Receive the next complete text message, answering pings on the way.
    /// A read timeout between frames leaves the connection usable; a
    /// timeout or error *inside* a frame, or an oversized frame, does not.
    pub fn recv_text(&mut self) -> Result<String, WsError> {
        if self.dead {
            return Err(WsError::Closed);
        }
        loop {
            let (fin, opcode, payload) = self.read_frame()?;
            match opcode {
                0x1 | 0x0 => {
                    if opcode == 0x1 {
                        self.fragments.clear();
                    }
                    if self.fragments.len().saturating_add(payload.len()) > MAX_FRAME_BYTES {
                        self.fragments.clear();
                        return Err(WsError::FrameBound);
                    }
                    self.fragments.extend_from_slice(&payload);
                    if fin {
                        let message = std::mem::take(&mut self.fragments);
                        return String::from_utf8(message).map_err(|_| WsError::Protocol);
                    }
                }
                0x2 => {
                    // Binary frames are not part of the DevTools protocol.
                    if fin {
                        return Err(WsError::Protocol);
                    }
                }
                0x8 => {
                    let _ = self.send_frame(0x8, &[]);
                    self.dead = true;
                    return Err(WsError::Closed);
                }
                0x9 => self.send_frame(0xA, &payload)?,
                0xA => {}
                _ => return Err(WsError::Protocol),
            }
        }
    }

    /// Best-effort close frame; the peer closing the TCP stream ends it.
    pub fn close(&mut self) {
        if !self.dead {
            let _ = self.send_frame(0x8, &[]);
        }
        self.dead = true;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WsError> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mask = self.next_mask();
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        self.stream.write_all(&frame).map_err(|_| WsError::Io)
    }

    fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>), WsError> {
        let mut head = [0u8; 2];
        // Before any byte of a frame has been consumed a timeout is benign:
        // the stream is still aligned on a frame boundary.
        match self.read_exact(&mut head) {
            Ok(()) => {}
            Err(WsError::Timeout) => return Err(WsError::Timeout),
            Err(err) => {
                self.dead = true;
                return Err(err);
            }
        }
        let inner = |this: &mut Self| -> Result<FrameBody, WsError> {
            let fin = head[0] & 0x80 != 0;
            let opcode = head[0] & 0x0f;
            let masked = head[1] & 0x80 != 0;
            let mut len = u64::from(head[1] & 0x7f);
            if len == 126 {
                let mut ext = [0u8; 2];
                this.read_exact(&mut ext)?;
                len = u64::from(u16::from_be_bytes(ext));
            } else if len == 127 {
                let mut ext = [0u8; 8];
                this.read_exact(&mut ext)?;
                len = u64::from_be_bytes(ext);
            }
            if len > MAX_FRAME_BYTES as u64 {
                return Err(WsError::FrameBound);
            }
            let mask = if masked {
                let mut key = [0u8; 4];
                this.read_exact(&mut key)?;
                Some(key)
            } else {
                None
            };
            let mut payload = vec![0u8; len as usize];
            this.read_exact(&mut payload)?;
            Ok((fin, opcode, payload, mask))
        };
        // Anything that fails once the header has been read leaves the
        // stream mid-frame: unrecoverable, so the connection is retired.
        let (fin, opcode, mut payload, mask) = match inner(self) {
            Ok(frame) => frame,
            Err(err) => {
                self.dead = true;
                return Err(err);
            }
        };
        if let Some(key) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= key[index % 4];
            }
        }
        Ok((fin, opcode, payload))
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), WsError> {
        self.stream.read_exact(buf).map_err(|err| match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => WsError::Timeout,
            io::ErrorKind::UnexpectedEof => WsError::Closed,
            _ => WsError::Io,
        })
    }

    fn nonce16(&mut self) -> [u8; 16] {
        let mut out = [0u8; 16];
        for chunk in out.chunks_mut(4) {
            chunk.copy_from_slice(&self.next_mask());
        }
        out
    }

    /// Masking key. RFC 6455 masks client frames so a hostile *page* cannot
    /// craft bytes that look like a frame to a proxy; on a loopback control
    /// channel to a browser we launched there is no proxy, so a fast
    /// xorshift stream seeded from the clock is all that is called for.
    fn next_mask(&mut self) -> [u8; 4] {
        let mut x = self.mask_seed;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.mask_seed = x;
        x.to_le_bytes()
    }
}

fn seed() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0x9e37_79b9);
    let pid = std::process::id();
    (nanos ^ pid.rotate_left(16) ^ 0x2545_f491).max(1)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(BASE64[(b0 >> 2) as usize] as char);
        out.push(BASE64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Standard base64 decode (padding optional, whitespace ignored).
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in text.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn base64_round_trips() {
        for sample in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0u8, 255, 128, 7, 9],
        ] {
            let encoded = base64_encode(sample);
            assert_eq!(base64_decode(&encoded).as_deref(), Some(sample));
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_decode("TWE"), Some(b"Ma".to_vec()));
        assert_eq!(base64_decode("!!"), None);
    }

    /// A tiny in-test server: accepts the upgrade, echoes one text frame
    /// (possibly fragmented on the way back), answers with a ping first.
    #[test]
    fn client_completes_the_upgrade_and_round_trips_text_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(stream.read(&mut byte).expect("read"), 1);
                request.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.starts_with("GET /devtools/browser/x HTTP/1.1\r\n"));
            assert!(text.contains("Sec-WebSocket-Key: "));
            stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
Connection: Upgrade\r\nSec-WebSocket-Accept: unchecked\r\n\r\n",
                )
                .expect("upgrade");
            // Read the masked client frame.
            let mut head = [0u8; 2];
            stream.read_exact(&mut head).expect("head");
            assert_eq!(head[0], 0x81);
            assert!(head[1] & 0x80 != 0, "client frames are masked");
            let len = usize::from(head[1] & 0x7f);
            let mut key = [0u8; 4];
            stream.read_exact(&mut key).expect("key");
            let mut payload = vec![0u8; len];
            stream.read_exact(&mut payload).expect("payload");
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= key[index % 4];
            }
            assert_eq!(payload, b"{\"id\":1}");
            // Ping first, then the echo split across two fragments.
            stream.write_all(&[0x89, 0x02, b'h', b'i']).expect("ping");
            let mut pong_head = [0u8; 2];
            stream.read_exact(&mut pong_head).expect("pong head");
            assert_eq!(pong_head[0], 0x8A);
            let mut rest = vec![0u8; usize::from(pong_head[1] & 0x7f) + 4];
            stream.read_exact(&mut rest).expect("pong body");
            stream
                .write_all(&[0x01, 0x04, b'{', b'"', b'i', b'd'])
                .expect("frag 1");
            stream
                .write_all(&[0x80, 0x04, b'"', b':', b'1', b'}'])
                .expect("frag 2");
            // Close.
            stream.write_all(&[0x88, 0x00]).expect("close");
        });
        let mut socket = WebSocket::connect(
            &format!("ws://127.0.0.1:{port}/devtools/browser/x"),
            Duration::from_secs(5),
        )
        .expect("connect");
        socket.send_text("{\"id\":1}").expect("send");
        assert_eq!(socket.recv_text().expect("echo"), "{\"id\":1}");
        assert_eq!(socket.recv_text().expect_err("closed"), WsError::Closed);
        server.join().expect("server");
    }

    #[test]
    fn a_silent_peer_is_a_timeout_not_a_hang() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(stream.read(&mut byte).expect("read"), 1);
                request.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n")
                .expect("upgrade");
            std::thread::sleep(Duration::from_millis(600));
        });
        let mut socket =
            WebSocket::connect(&format!("ws://127.0.0.1:{port}/"), Duration::from_secs(5))
                .expect("connect");
        socket
            .set_read_timeout(Duration::from_millis(100))
            .expect("timeout");
        let started = std::time::Instant::now();
        assert_eq!(socket.recv_text().expect_err("silent"), WsError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().expect("server");
    }

    #[test]
    fn a_timeout_inside_a_frame_retires_the_connection_but_between_frames_does_not() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(stream.read(&mut byte).expect("read"), 1);
                request.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n")
                .expect("upgrade");
            // A whole frame, then a pause, then half a frame and silence.
            stream.write_all(&[0x81, 0x02, b'o', b'k']).expect("frame");
            std::thread::sleep(Duration::from_millis(300));
            stream.write_all(&[0x81, 0x04, b'h', b'a']).expect("half");
            std::thread::sleep(Duration::from_millis(600));
        });
        let mut socket =
            WebSocket::connect(&format!("ws://127.0.0.1:{port}/"), Duration::from_secs(5))
                .expect("connect");
        socket
            .set_read_timeout(Duration::from_millis(100))
            .expect("timeout");
        assert_eq!(socket.recv_text().expect("first"), "ok");
        // Between frames: a timeout, and the socket is still fine.
        assert_eq!(socket.recv_text().expect_err("pause"), WsError::Timeout);
        assert!(!socket.is_dead());
        // Inside a frame: the same timeout retires it.
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(
            socket.recv_text().expect_err("half frame"),
            WsError::Timeout
        );
        assert!(socket.is_dead());
        assert_eq!(socket.recv_text().expect_err("dead"), WsError::Closed);
        assert_eq!(socket.send_text("x").expect_err("dead"), WsError::Closed);
        server.join().expect("server");
    }

    #[test]
    fn a_non_upgrade_response_fails_the_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(stream.read(&mut byte).expect("read"), 1);
                request.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .expect("404");
        });
        let err = WebSocket::connect(&format!("ws://127.0.0.1:{port}/"), Duration::from_secs(5))
            .expect_err("no upgrade");
        assert_eq!(err, WsError::Handshake);
        server.join().expect("server");
    }
}
