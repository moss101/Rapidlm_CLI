//! A loopback HTTP server the authorization flow's tests talk to (tests only):
//! every request is logged and answered by a closure, so one stub can play an
//! MCP server, an authorization server or the browser's far end.

// Each test file uses the part of it that it needs.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: String,
    /// The path and query, as sent.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The path without its query.
    pub fn route(&self) -> &str {
        self.path.split('?').next().unwrap_or_default()
    }

    /// The query pairs of the path, decoded.
    pub fn query(&self) -> Vec<(String, String)> {
        self.path
            .split_once('?')
            .map(|(_, query)| crate::mcp_oauth::parse_query(query))
            .unwrap_or_default()
    }

    /// The form fields of the body, decoded.
    pub fn form(&self) -> Vec<(String, String)> {
        crate::mcp_oauth::parse_query(&String::from_utf8_lossy(&self.body))
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub(crate) struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn text(status: u16, content_type: &str, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), content_type.to_owned())],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn redirect(status: u16, location: &str) -> Self {
        Self {
            status,
            headers: vec![("Location".to_owned(), location.to_owned())],
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn not_found() -> Self {
        Self::json(404, r#"{"error":"not_found"}"#)
    }
}

pub(crate) struct HttpStub {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

type Handler = dyn Fn(&Request, &str) -> Response + Send + Sync;

impl HttpStub {
    pub fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        Self::start_with_self(move |request, _base| handler(request))
    }

    /// A stub whose handler is told the stub's own base URL.
    pub fn start_with_self(
        handler: impl Fn(&Request, &str) -> Response + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let base = format!("http://{addr}");
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let thread = {
            let log = Arc::clone(&log);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let log = Arc::clone(&log);
                            let handler = Arc::clone(&handler);
                            let base = base.clone();
                            std::thread::spawn(move || serve(stream, &log, &handler, &base));
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            addr,
            log,
            stop,
            thread: Some(thread),
        }
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base())
    }

    pub fn authority(&self) -> String {
        self.addr.to_string()
    }

    /// Every request so far, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The requests whose route is `route`.
    pub fn requests_to(&self, route: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| request.route() == route)
            .collect()
    }
}

impl Drop for HttpStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut stream: TcpStream, log: &Mutex<Vec<Request>>, handler: &Arc<Handler>, base: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(request.clone());
    let response = handler(&request, base);
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    ));
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 256 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let path = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}
