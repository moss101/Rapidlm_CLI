//! The redirect's end of the authorization flow (EXT-02-2 §4.3 steps 6 and 7):
//! a loopback listener that takes exactly one valid callback, and the browser
//! opener.
//!
//! The listener is bound to `127.0.0.1` and nothing else. It accepts one `GET`
//! of `/callback` whose `Host` is the address it is bound to and whose `state`
//! is the one the flow made; that request ends the wait. Anything else — the
//! wrong path, method, host or state — is answered with a static page that
//! echoes nothing and does not end the wait; the fifth such request ends it
//! unsuccessfully. A callback pasted on standard input (`--no-browser`, over
//! SSH) goes through the same judgment.
//!
//! The opener runs the platform's program **without a shell**, the URL as one
//! argument, so nothing in a URL can be read as anything but a URL. Where there
//! is no such program, or `RAPIDLM_NO_BROWSER` is set, that is a typed
//! unavailability and the caller prints the URL instead.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::mcp_oauth::{Callback, judge_callback, query_of_pasted};

/// The path the redirect arrives at.
pub const CALLBACK_PATH: &str = "/callback";

/// How many requests that are not the redirect end the wait.
pub const MAX_INVALID_REQUESTS: usize = 5;

/// The most bytes of a request read.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// How long one connection may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the wait looks for a connection, a pasted line and the deadline.
const POLL: Duration = Duration::from_millis(20);

/// What the person sees in the browser once the redirect has arrived.
const DONE_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>RapidLM</title>\
<p>You can close this window and return to RapidLM.</p>";

/// What anything else that reaches the listener gets: nothing it sent comes
/// back.
const REFUSED_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>RapidLM</title>\
<p>This address is not part of an authorization in progress.</p>";

/// Why the listener could not be bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenError {
    /// A port the entry named is in use (or not allowed).
    PortInUse(u16),
    /// Nothing could be bound.
    Failed(String),
}

impl std::fmt::Display for ListenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PortInUse(port) => write!(
                f,
                "port {port}, which this server's `auth.callback_port` names, cannot be listened \
on (is another program using it?)"
            ),
            Self::Failed(why) => write!(f, "no port on 127.0.0.1 could be listened on: {why}"),
        }
    }
}

/// How the wait ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Waited {
    /// The redirect arrived.
    Callback(Callback),
    /// Five requests that were not the redirect.
    TooManyInvalid,
    /// The time ran out.
    TimedOut,
    /// The cancel flag was raised.
    Cancelled,
}

/// A listener on `127.0.0.1`.
pub struct Listener {
    inner: TcpListener,
    port: u16,
}

impl Listener {
    /// Listen on `port`, or on any free port when `None`.
    pub fn bind(port: Option<u16>) -> Result<Self, ListenError> {
        let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port.unwrap_or(0)));
        let inner = TcpListener::bind(address).map_err(|err| match port {
            Some(port) if err.kind() == std::io::ErrorKind::AddrInUse => {
                ListenError::PortInUse(port)
            }
            Some(port) => ListenError::Failed(format!("port {port}: {err}")),
            None => ListenError::Failed(err.to_string()),
        })?;
        inner
            .set_nonblocking(true)
            .map_err(|err| ListenError::Failed(err.to_string()))?;
        let port = inner
            .local_addr()
            .map_err(|err| ListenError::Failed(err.to_string()))?
            .port();
        Ok(Self { inner, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The address the listener is bound to.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.inner.local_addr().ok()
    }

    /// The redirect URI a client registers and asks for.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{CALLBACK_PATH}", self.port)
    }

    /// Wait for the redirect for `state`. `pasted` carries callback URLs a
    /// person typed (`--no-browser`); `notice` is told, in words, of a pasted
    /// line that is not it. `cancelled` is looked at every poll.
    pub fn wait(
        self,
        state: &str,
        timeout: Duration,
        pasted: Option<&Receiver<String>>,
        cancelled: &dyn Fn() -> bool,
        notice: &mut dyn FnMut(&str),
    ) -> Waited {
        let deadline = Instant::now() + timeout;
        let mut invalid = 0usize;
        loop {
            if cancelled() {
                return Waited::Cancelled;
            }
            if Instant::now() >= deadline {
                return Waited::TimedOut;
            }
            if let Some(pasted) = pasted {
                match pasted.try_recv() {
                    Ok(line) => {
                        match query_of_pasted(&line).map(|query| judge_callback(query, state)) {
                            Some(Ok(callback)) => return Waited::Callback(callback),
                            _ => {
                                invalid += 1;
                                notice(
                                    "that is not the callback for this authorization; paste the \
address the browser was sent to",
                                );
                                if invalid >= MAX_INVALID_REQUESTS {
                                    return Waited::TooManyInvalid;
                                }
                            }
                        }
                        continue;
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
                }
            }
            match self.inner.accept() {
                Ok((stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        // Bound to loopback, so this cannot happen; refuse
                        // anyway rather than reason about it.
                        continue;
                    }
                    match self.serve(stream, state) {
                        Some(callback) => return Waited::Callback(callback),
                        None => {
                            invalid += 1;
                            if invalid >= MAX_INVALID_REQUESTS {
                                return Waited::TooManyInvalid;
                            }
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                }
                Err(_) => std::thread::sleep(POLL),
            }
        }
    }

    /// Answer one connection: the redirect for `state`, or `None` for anything
    /// else (which is answered with the static page).
    fn serve(&self, mut stream: TcpStream, state: &str) -> Option<Callback> {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
        let request = read_head(&mut stream);
        let judged = request
            .as_deref()
            .and_then(|head| judge_request(head, self.port, state));
        match judged {
            Some(callback) => {
                respond(&mut stream, 200, "OK", DONE_PAGE);
                Some(callback)
            }
            None => {
                respond(&mut stream, 400, "Bad Request", REFUSED_PAGE);
                None
            }
        }
    }
}

/// The request line and headers, up to the blank line or the bound.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() >= MAX_REQUEST_BYTES {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    }
    String::from_utf8(buffer).ok()
}

/// Whether `head` is the redirect: a `GET` of the callback path, the right
/// `Host`, the right `state`, carrying a `code` or an `error`.
fn judge_request(head: &str, port: u16, state: &str) -> Option<Callback> {
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    if request_line.next()? != "GET" {
        return None;
    }
    let target = request_line.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return None;
    }
    // The `Host` is the address this listener is bound to, exactly: a browser
    // reached through another name (a rebinding page) is not the redirect.
    let host_ok = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("host") && value.trim() == format!("127.0.0.1:{port}")
        });
    if !host_ok {
        return None;
    }
    judge_callback(query, state).ok()
}

fn respond(stream: &mut TcpStream, status: u16, reason: &str, page: &str) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
Connection: close\r\n\r\n",
        page.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(page.as_bytes());
    let _ = stream.flush();
}

// --- the opener ---------------------------------------------------------------------

/// Why the browser was not opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// There is no opener to run, or the person said not to: the URL is
    /// printed instead.
    Unavailable(String),
    /// The opener could not be started.
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

/// Opens a URL in the person's browser.
pub trait Opener {
    fn open(&self, url: &str) -> Result<(), OpenError>;
}

/// The program and arguments that open `url` on `os` (`std::env::consts::OS`),
/// with no shell: the URL is one argument. `None` on a platform with none.
pub fn opener_argv(os: &str, url: &str) -> Option<(&'static str, Vec<String>)> {
    match os {
        "macos" => Some(("open", vec![url.to_owned()])),
        // Not `cmd /c start`: a shell would read the `&` of a query.
        "windows" => Some((
            "rundll32",
            vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()],
        )),
        "linux" | "freebsd" | "netbsd" | "openbsd" | "dragonfly" => {
            Some(("xdg-open", vec![url.to_owned()]))
        }
        _ => None,
    }
}

/// The platform's opener, found on the `PATH` of `env`.
pub struct PlatformOpener {
    env: Vec<(String, String)>,
    os: &'static str,
}

impl PlatformOpener {
    pub fn new(env: Vec<(String, String)>) -> Self {
        Self {
            env,
            os: std::env::consts::OS,
        }
    }

    #[cfg(test)]
    pub fn for_os(env: Vec<(String, String)>, os: &'static str) -> Self {
        Self { env, os }
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The executable `program` names on the `PATH` of this environment.
    fn find(&self, program: &str) -> Option<PathBuf> {
        let path = self.var("PATH")?;
        std::env::split_paths(path).find_map(|dir| {
            let mut candidates = vec![dir.join(program)];
            if cfg!(windows) {
                candidates.push(dir.join(format!("{program}.exe")));
            }
            candidates
                .into_iter()
                .find(|candidate| is_executable(candidate))
        })
    }
}

fn is_executable(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

impl Opener for PlatformOpener {
    fn open(&self, url: &str) -> Result<(), OpenError> {
        if self
            .var("RAPIDLM_NO_BROWSER")
            .is_some_and(|v| !v.is_empty())
        {
            return Err(OpenError::Unavailable(
                "RAPIDLM_NO_BROWSER is set".to_owned(),
            ));
        }
        let Some((program, args)) = opener_argv(self.os, url) else {
            return Err(OpenError::Unavailable(format!(
                "there is no known way to open a browser on {}",
                self.os
            )));
        };
        let Some(path) = self.find(program) else {
            return Err(OpenError::Unavailable(format!(
                "`{program}` was not found on the PATH"
            )));
        };
        let mut child = std::process::Command::new(path)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|err| OpenError::Failed(format!("`{program}` could not be started: {err}")))?;
        // Reaped when it ends; some openers stay until the browser does.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "state-abc_123";

    fn get(port: u16, target: &str, host: &str) -> String {
        request(
            port,
            &format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n"),
        )
    }

    fn request(port: u16, raw: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        // The listener may answer and close before everything is sent.
        let _ = stream.write_all(raw.as_bytes());
        let mut out = String::new();
        let _ = stream.read_to_string(&mut out);
        out
    }

    fn wait_in_thread(listener: Listener, timeout: Duration) -> std::thread::JoinHandle<Waited> {
        std::thread::spawn(move || listener.wait(STATE, timeout, None, &|| false, &mut |_| {}))
    }

    #[test]
    fn the_listener_is_bound_to_127_0_0_1_only() {
        let listener = Listener::bind(None).expect("bind");
        let addr = listener.local_addr().expect("addr");
        assert_eq!(addr.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(addr.port(), 0);
        assert_eq!(listener.port(), addr.port());
        assert_eq!(
            listener.redirect_uri(),
            format!("http://127.0.0.1:{}/callback", addr.port())
        );
    }

    #[test]
    fn a_taken_port_the_entry_named_is_a_typed_error_naming_it() {
        let holder = Listener::bind(None).expect("bind");
        let port = holder.port();
        let err = Listener::bind(Some(port)).err().expect("in use");
        assert_eq!(err, ListenError::PortInUse(port));
        assert!(err.to_string().contains(&port.to_string()));
        // An ephemeral request is never in use.
        assert!(Listener::bind(None).is_ok());
    }

    #[test]
    fn the_matching_redirect_ends_the_wait_with_its_code_and_answers_a_static_page() {
        let listener = Listener::bind(None).expect("bind");
        let port = listener.port();
        let waiting = wait_in_thread(listener, Duration::from_secs(10));
        let answer = get(
            port,
            &format!("/callback?code=CODE-CANARY&state={STATE}&iss=https%3A%2F%2Fas.example"),
            &format!("127.0.0.1:{port}"),
        );
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.contains("close this window"), "{answer}");
        assert!(!answer.contains("CODE-CANARY"), "the page echoes nothing");
        match waiting.join().expect("thread") {
            Waited::Callback(Callback::Code { code, iss }) => {
                assert_eq!(code.expose(), "CODE-CANARY");
                assert_eq!(iss.as_deref(), Some("https://as.example"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_refusal_from_the_person_is_a_callback_too() {
        let listener = Listener::bind(None).expect("bind");
        let port = listener.port();
        let waiting = wait_in_thread(listener, Duration::from_secs(10));
        get(
            port,
            &format!("/callback?error=access_denied&state={STATE}"),
            &format!("127.0.0.1:{port}"),
        );
        assert_eq!(
            waiting.join().expect("thread"),
            Waited::Callback(Callback::Denied)
        );
    }

    #[test]
    fn requests_that_are_not_the_redirect_are_answered_and_do_not_end_the_wait() {
        let listener = Listener::bind(None).expect("bind");
        let port = listener.port();
        let host = format!("127.0.0.1:{port}");
        let waiting = wait_in_thread(listener, Duration::from_secs(10));
        let good_query = format!("code=C&state={STATE}");
        for (label, raw) in [
            (
                "wrong state",
                get(port, "/callback?code=C&state=nope", &host),
            ),
            ("no state", get(port, "/callback?code=C", &host)),
            (
                "wrong path",
                get(port, &format!("/other?{good_query}"), &host),
            ),
            (
                "wrong host",
                get(port, &format!("/callback?{good_query}"), "evil.example"),
            ),
        ] {
            assert!(raw.starts_with("HTTP/1.1 400"), "{label}: {raw}");
            assert!(raw.contains("not part of an authorization"), "{label}");
            assert!(
                !raw.contains(&good_query) && !raw.contains("nope"),
                "{label}: echoes"
            );
        }
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            !waiting.is_finished(),
            "four invalid requests did not end it"
        );
        // The real one still does.
        get(port, &format!("/callback?{good_query}"), &host);
        assert!(matches!(
            waiting.join().expect("thread"),
            Waited::Callback(Callback::Code { .. })
        ));
    }

    #[test]
    fn a_host_that_is_not_the_bound_address_is_not_the_redirect() {
        let listener = Listener::bind(None).expect("bind");
        let port = listener.port();
        let waiting = wait_in_thread(listener, Duration::from_secs(10));
        let query = format!("/callback?code=C&state={STATE}");
        for host in [
            "127.0.0.1:1".to_owned(),
            format!("localhost:{port}"),
            "127.0.0.1".to_owned(),
            format!("127.0.0.1:{port}.evil.example"),
        ] {
            let answer = get(port, &query, &host);
            assert!(answer.starts_with("HTTP/1.1 400"), "{host}: {answer}");
        }
        // Four down; one with no Host header at all is the fifth.
        let answer = request(port, &format!("GET {query} HTTP/1.1\r\n\r\n"));
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert_eq!(waiting.join().expect("thread"), Waited::TooManyInvalid);
    }

    #[test]
    fn a_method_other_than_get_is_not_the_redirect() {
        let listener = Listener::bind(None).expect("bind");
        let port = listener.port();
        let waiting = wait_in_thread(listener, Duration::from_secs(10));
        for method in ["POST", "PUT", "DELETE", "HEAD"] {
            let answer = request(
                port,
                &format!(
                    "{method} /callback?code=C&state={STATE} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
Content-Length: 0\r\n\r\n"
                ),
            );
            assert!(answer.starts_with("HTTP/1.1 400"), "{method}: {answer}");
        }
        // Four invalid so far; the fifth ends the wait. A valid request would
        // have been accepted instead.
        let answer = get(port, "/nothing", &format!("127.0.0.1:{port}"));
        assert!(answer.starts_with("HTTP/1.1 400"));
        assert_eq!(waiting.join().expect("thread"), Waited::TooManyInvalid);
    }

    #[test]
    fn the_fifth_invalid_request_aborts_and_a_valid_one_before_it_wins() {
        let listener = Listener::bind(None).expect("bind");
        let port = listener.port();
        let host = format!("127.0.0.1:{port}");
        let waiting = wait_in_thread(listener, Duration::from_secs(10));
        for _ in 0..MAX_INVALID_REQUESTS - 1 {
            get(port, "/callback?code=C&state=wrong", &host);
        }
        // Four invalid requests have not ended it: a valid one still does.
        get(port, &format!("/callback?code=OK&state={STATE}"), &host);
        match waiting.join().expect("thread") {
            Waited::Callback(Callback::Code { code, .. }) => assert_eq!(code.expose(), "OK"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_request_that_never_finishes_or_is_not_http_does_not_end_the_wait_early() {
        let listener = Listener::bind(None).expect("bind");
        let port = listener.port();
        let waiting = wait_in_thread(listener, Duration::from_secs(10));
        // Garbage, then an oversized head: each is one invalid request.
        request(port, "\u{0}\u{1}not http at all\r\n\r\n");
        request(
            port,
            &format!("GET /callback?{} HTTP/1.1\r\n\r\n", "a".repeat(20_000)),
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiting.is_finished(), "two bad requests did not end it");
        // And the real one still gets through.
        get(
            port,
            &format!("/callback?code=OK&state={STATE}"),
            &format!("127.0.0.1:{port}"),
        );
        assert!(matches!(
            waiting.join().expect("thread"),
            Waited::Callback(Callback::Code { .. })
        ));
    }

    #[test]
    fn the_wait_ends_at_the_timeout_and_when_cancelled_and_stores_nothing() {
        let listener = Listener::bind(None).expect("bind");
        let started = Instant::now();
        let outcome = listener.wait(
            STATE,
            Duration::from_millis(150),
            None,
            &|| false,
            &mut |_| {},
        );
        assert_eq!(outcome, Waited::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(started.elapsed() < Duration::from_secs(5));
        let listener = Listener::bind(None).expect("bind");
        let outcome = listener.wait(STATE, Duration::from_secs(30), None, &|| true, &mut |_| {});
        assert_eq!(outcome, Waited::Cancelled);
    }

    #[test]
    fn a_pasted_callback_is_judged_like_a_request() {
        let (send, receive) = std::sync::mpsc::channel::<String>();
        let listener = Listener::bind(None).expect("bind");
        let port = listener.port();
        send.send("this is not a url".to_owned()).unwrap();
        send.send(format!(
            "http://127.0.0.1:{port}/callback?code=C&state=wrong"
        ))
        .unwrap();
        send.send(format!(
            "http://127.0.0.1:{port}/callback?code=PASTED&state={STATE}"
        ))
        .unwrap();
        let mut notices = Vec::new();
        let outcome = listener.wait(
            STATE,
            Duration::from_secs(10),
            Some(&receive),
            &|| false,
            &mut |text| notices.push(text.to_owned()),
        );
        match outcome {
            Waited::Callback(Callback::Code { code, .. }) => assert_eq!(code.expose(), "PASTED"),
            other => panic!("{other:?}"),
        }
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].contains("not the callback"));
    }

    #[test]
    fn five_wrong_pastes_end_the_wait() {
        let (send, receive) = std::sync::mpsc::channel::<String>();
        let listener = Listener::bind(None).expect("bind");
        for _ in 0..MAX_INVALID_REQUESTS {
            send.send("?code=C&state=wrong".to_owned()).unwrap();
        }
        let outcome = listener.wait(
            STATE,
            Duration::from_secs(10),
            Some(&receive),
            &|| false,
            &mut |_| {},
        );
        assert_eq!(outcome, Waited::TooManyInvalid);
    }

    #[test]
    fn the_opener_argv_has_the_url_as_one_argument_and_no_shell() {
        let url = "https://as.example/authorize?response_type=code&state=a b;rm -rf /&x=$(id)";
        for (os, program) in [
            ("macos", "open"),
            ("linux", "xdg-open"),
            ("freebsd", "xdg-open"),
            ("windows", "rundll32"),
        ] {
            let (found, args) = opener_argv(os, url).expect(os);
            assert_eq!(found, program);
            assert_eq!(
                args.last().map(String::as_str),
                Some(url),
                "{os}: whole, last"
            );
            assert_eq!(
                args.iter().filter(|a| a.contains("response_type")).count(),
                1,
                "{os}: the URL is exactly one argument"
            );
            assert!(!["sh", "bash", "cmd", "cmd.exe"].contains(&found), "{os}");
        }
        assert!(opener_argv("plan9", url).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn a_fake_opener_on_the_path_is_run_without_a_shell_with_the_url_as_its_one_argument() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "rapid-opener-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join("argv.txt");
        let program = if std::env::consts::OS == "macos" {
            "open"
        } else {
            "xdg-open"
        };
        let script = dir.join(program);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$#\" > '{0}'\nprintf '%s' \"$1\" >> '{0}'\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let url = "http://127.0.0.1:5/authorize?a=1&b=two words;touch NOPE&c=$(id)`id`";
        let opener = PlatformOpener::new(vec![("PATH".to_owned(), dir.display().to_string())]);
        opener.open(url).expect("opened");
        let mut seen = String::new();
        for _ in 0..200 {
            seen = std::fs::read_to_string(&record).unwrap_or_default();
            if seen.contains("authorize") {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let (count, argument) = seen.split_once('\n').expect(&seen);
        assert_eq!(count, "1", "one argument");
        assert_eq!(argument, url, "the URL, whole and unread by any shell");
        assert!(!dir.join("NOPE").exists());
    }

    #[test]
    fn no_opener_or_a_person_who_said_no_is_a_typed_unavailability() {
        // Nothing on the PATH: typed, on every platform.
        let none = PlatformOpener::new(vec![("PATH".to_owned(), String::new())]);
        let err = none.open("https://as.example/").unwrap_err();
        assert!(matches!(err, OpenError::Unavailable(_)), "{err:?}");
        assert!(err.to_string().contains("not found on the PATH"), "{err}");
        // No PATH at all.
        assert!(matches!(
            PlatformOpener::new(Vec::new()).open("https://as.example/"),
            Err(OpenError::Unavailable(_))
        ));
        // A platform with no known opener.
        let odd = PlatformOpener::for_os(vec![("PATH".to_owned(), "/bin".to_owned())], "plan9");
        assert!(matches!(
            odd.open("https://as.example/"),
            Err(OpenError::Unavailable(why)) if why.contains("plan9")
        ));
        // RAPIDLM_NO_BROWSER wins even when an opener exists.
        let said_no = PlatformOpener::new(vec![
            ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
            ("RAPIDLM_NO_BROWSER".to_owned(), "1".to_owned()),
        ]);
        let err = said_no.open("https://as.example/").unwrap_err();
        assert_eq!(
            err,
            OpenError::Unavailable("RAPIDLM_NO_BROWSER is set".to_owned())
        );
        // An empty value does not count as set.
        let empty = PlatformOpener::new(vec![
            ("PATH".to_owned(), String::new()),
            ("RAPIDLM_NO_BROWSER".to_owned(), String::new()),
        ]);
        assert!(
            !empty
                .open("https://as.example/")
                .unwrap_err()
                .to_string()
                .contains("RAPIDLM_NO_BROWSER")
        );
    }
}
