//! The CI key-isolation proxy's contract (`examples/ci/key-proxy.py`),
//! tested for real: the examples directory ships as product surface, and
//! its one security property — the credential lives only in the proxy —
//! is only as good as its behavior. The tests below drive the actual
//! script against a local upstream stub and assert what matters:
//!
//! 1. the caller's (dummy) authorization is *replaced* by the real key
//!    upstream — the token rapid holds in CI is useless outside;
//! 2. `CONNECT` is refused — a hijacked run cannot pivot the proxy into
//!    an open relay;
//! 3. the body and the status round-trip.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ProxyProcess(Child);

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is the crate root (apps/rapid); the repo root is
    // its parent — stable wherever cargo places the test binary.
    // apps/rapid -> the workspace root, where examples/ lives.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .expect("the crate has a grandparent")
}

fn start_proxy(upstream_port: u16) -> (u16, ProxyProcess) {
    // Reserve a port by binding, then let it go: the placeholder must be
    // closed before the proxy binds it, or the proxy dies on EADDRINUSE
    // while the readiness probe happily connects to the placeholder.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let script = repo_root().join("examples").join("ci").join("key-proxy.py");
    assert!(script.is_file(), "{} is missing", script.display());
    let child = Command::new("python3")
        .arg(script)
        .arg(port.to_string())
        .arg(format!("http://127.0.0.1:{upstream_port}/v1"))
        .env("KEY", "the-real-ci-key")
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 starts the proxy");
    let proxy = ProxyProcess(child);
    // Wait for the listener to accept.
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (port, proxy);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the proxy never came up");
}

/// One captured upstream request: its Authorization header and body.
struct UpstreamSeen {
    authorization: Option<String>,
    body: String,
}

/// Serve exactly one POST, echoing the Authorization header it saw, and
/// die (the tests are one request each).
fn serve_one_upstream(listener: TcpListener) -> UpstreamSeen {
    let (mut stream, _) = listener.accept().expect("upstream accepts");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = stream.read(&mut byte) {
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head_text = String::from_utf8_lossy(&head).into_owned();
    let length: usize = head_text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim().eq_ignore_ascii_case("content-length"))
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut body).expect("upstream body");
    }
    let authorization = head_text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_owned())
    });
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{{\"saw_key\":{}}}",
        21,
        authorization
            .as_deref()
            .is_some_and(|v| v == "Bearer the-real-ci-key")
    );
    stream
        .write_all(response.as_bytes())
        .expect("upstream answers");
    UpstreamSeen {
        authorization,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn http_post(port: u16, path: &str, authorization: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to proxy");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {authorization}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("send");
    read_to_end_bounded(stream)
}

fn http_connect(port: u16, target: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to proxy");
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("send");
    read_to_end_bounded(stream)
}

/// `read_to_end` with a deadline: the proxy answers and closes; if it
/// ever keeps the connection open, the test fails in seconds instead of
/// hanging the suite.
fn read_to_end_bounded(mut stream: TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => response.extend_from_slice(&chunk[..read]),
        }
    }
    String::from_utf8_lossy(&response).into_owned()
}

#[test]
fn the_proxy_replaces_the_callers_token_with_the_real_key_upstream() {
    let upstream = TcpListener::bind("127.0.0.1:0").expect("bind");
    let upstream_port = upstream.local_addr().expect("addr").port();
    let (port, _proxy) = start_proxy(upstream_port);
    let server = std::thread::spawn(move || serve_one_upstream(upstream));

    // What rapid in CI holds: a dummy token. It must never reach the
    // provider — the real key replaces it, whatever it was.
    let response = http_post(
        port,
        "/v1/chat/completions",
        "Bearer ci-dummy-token",
        r#"{"prompt":"hello"}"#,
    );
    let seen = server.join().expect("upstream served");
    assert_eq!(
        seen.authorization.as_deref(),
        Some("Bearer the-real-ci-key"),
        "the upstream sees the proxy's key, not the caller's"
    );
    assert_eq!(seen.body, r#"{"prompt":"hello"}"#, "the body round-trips");
    assert!(
        response.contains("200 OK") && response.contains("\"saw_key\":true"),
        "the status and body round-trip: {response}"
    );
}

#[test]
fn connect_is_refused_so_the_proxy_is_not_an_open_relay() {
    let upstream = TcpListener::bind("127.0.0.1:0").expect("bind");
    let upstream_port = upstream.local_addr().expect("addr").port();
    let (port, _proxy) = start_proxy(upstream_port);
    let response = http_connect(port, "evil.example:443");
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "CONNECT is refused: {response}"
    );
}
