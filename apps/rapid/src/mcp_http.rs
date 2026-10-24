//! Production HTTP io for MCP Streamable HTTP (delivery goal §5).
//!
//! `crates/mcp`'s `StreamableHttpTransport` owns the protocol: session ids,
//! protocol-version headers, redirect/origin re-authorization, frame caps.
//! What it deliberately does not own is the socket. This module is the io
//! adapter that plugs the real HTTP/1.1 + rustls client from `llm-router`
//! (the same audited path provider calls use: SSRF guards, response bounds,
//! honor-the-cancel-token) into that transport, plus the configured-header
//! authentication boundary — `headers` from the server config, or a bearer
//! token, are attached verbatim at this boundary and nowhere else.
//!
//! Every dial goes through an egress gate (S10) and leaves a receipt in the
//! project's receipt log, the way web search and the update check do.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use capability_broker::CancellationToken;
use llm_router::providers::dial::DialGate;
use llm_router::providers::openai_compatible::{Http1Transport, StaticWireAuth};
use mcp::transport::{HttpRequest, HttpResponse, StreamableHttpIo, TransportError};
use security::ConsumedConnect;

use crate::provider_egress::ProviderEgress;

/// Most bytes of a reply body this adapter reads; the transport bounds each
/// frame inside it further.
const MAX_REPLY_BYTES: usize = mcp::MAX_EVENT_STREAM_BYTES;

/// MCP HTTP io over llm-router's blocking HTTP/1.1 + TLS client.
pub struct RapidHttpIo {
    /// Extra headers from the server's config entry (tenant routing, auth),
    /// attached verbatim to every exchange. A header named `Authorization`
    /// is the whole credential: it is sent as written, once.
    pub headers: Vec<(String, String)>,
    /// Bearer token for `Authorization`, resolved from the entry's
    /// `bearer_token_env` at connect. Never stored in the entry itself.
    pub bearer: Option<String>,
    /// Where egress receipts are appended (`.rapidlm/egress-receipts.jsonl`
    /// under this root). `None` keeps them in memory only, for tests.
    pub receipt_root: Option<PathBuf>,
    /// The egress gate for the configured origin.
    pub gate: Option<Arc<ProviderEgress>>,
}

impl RapidHttpIo {
    /// The `Authorization` value to send: a configured header wins over a
    /// bearer token only because the two are never both configured (the
    /// config loader refuses that); a bare `Authorization: Bearer` is never
    /// invented for a server that has no credential.
    fn authorization(&self) -> Option<String> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.clone())
            .or_else(|| {
                self.bearer
                    .as_ref()
                    .filter(|token| !token.is_empty())
                    .map(|token| format!("Bearer {token}"))
            })
    }
}

impl StreamableHttpIo for RapidHttpIo {
    fn exchange(
        &mut self,
        request: &HttpRequest<'_>,
        _grant: &ConsumedConnect,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        let mut transport = Http1Transport::with_limits(
            // The wire credential below is written by `post_raw_authorized`;
            // this value only satisfies the transport's constructor and is
            // never put on the wire.
            StaticWireAuth::bearer("unused".to_owned())
                .map_err(|_err| TransportError::HandshakeFailed)?,
            timeout,
            MAX_REPLY_BYTES,
        );
        if let Some(gate) = &self.gate {
            transport = transport.with_dial_gate(Arc::clone(gate) as Arc<dyn DialGate>);
        }

        let mut headers: Vec<(String, String)> = vec![
            (
                "Accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            (
                "MCP-Protocol-Version".to_owned(),
                request.protocol_version().as_str().to_owned(),
            ),
        ];
        if let Some(session) = request.session_id() {
            headers.push(("mcp-session-id".to_owned(), session.as_str().to_owned()));
        }
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("authorization") {
                continue;
            }
            headers.push((name.clone(), value.clone()));
        }
        let authorization = self.authorization();

        // Bridge the capability-broker token into the router's: the watcher
        // cancels the router token the moment the broker's fires, and ends
        // with the exchange (`done`) rather than outliving it.
        let router_cancel = llm_router::provider::CancellationToken::new();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let router = router_cancel.clone();
            let broker = cancel.clone();
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                while !done.load(std::sync::atomic::Ordering::Acquire)
                    && !broker.is_cancelled()
                    && !router.is_cancelled()
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if broker.is_cancelled() {
                    router.cancel();
                }
            })
        };
        let result = transport.post_raw_authorized(
            &request.url(),
            &headers,
            request.body(),
            authorization.as_deref(),
            &router_cancel,
        );
        done.store(true, std::sync::atomic::Ordering::Release);
        let _ = watcher.join();
        self.record_receipts();
        let response = result.map_err(map_provider_error)?;

        // Map the raw response back into the transport's typed shape: the
        // session id, redirect location, content type and challenge come
        // from headers only.
        let session_id = response.header("mcp-session-id").map(str::to_owned);
        let location = response.header("location").map(str::to_owned);
        let content_type = response.header("content-type").map(str::to_owned);
        // Every challenge line, joined: a server may send one per scheme or
        // a second line for the scope.
        let challenge_lines: Vec<&str> = response
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("www-authenticate"))
            .map(|(_, value)| value.as_str())
            .collect();
        let challenge = (!challenge_lines.is_empty()).then(|| challenge_lines.join(", "));
        Ok(
            HttpResponse::new(response.status, session_id, location, response.body)?
                .with_content_type(content_type)
                .with_www_authenticate(challenge),
        )
    }
}

impl RapidHttpIo {
    /// Append the decisions the gate made since the last call to the
    /// project's receipt log.
    fn record_receipts(&self) {
        let (Some(gate), Some(root)) = (&self.gate, &self.receipt_root) else {
            return;
        };
        let receipts: Vec<crate::web_search::Receipt> = gate
            .drain_receipts()
            .into_iter()
            .map(|receipt| crate::web_search::Receipt {
                tool: "mcp",
                allowed: receipt.allowed,
                dialled: receipt.dialled,
                reason: receipt.reason,
            })
            .collect();
        crate::web_search::record(root, &receipts);
    }
}

fn map_provider_error(err: llm_router::provider::ProviderError) -> TransportError {
    use llm_router::provider::ProviderError as P;
    match err {
        P::Cancelled => TransportError::Cancelled,
        P::BoundExceeded => TransportError::FrameTooLarge,
        P::Connection => TransportError::HandshakeFailed,
        _ => TransportError::HandshakeFailed,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use crate::exec_tools::{
        McpConnectError, McpHttpEndpoint, McpServerConfig, connect_mcp_server_with_env,
    };

    const SECRET: &str = "tok-5e7a1c9d-do-not-print";

    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) enum CallReply {
        #[default]
        Json,
        EventStream,
        /// The tool result text is the `Authorization` header the request
        /// carried: a server that echoes a credential back.
        EchoAuthorization,
        /// The same echo after this many bytes of padding, so a caller can put
        /// the credential across a byte cap on a tool result.
        EchoAfterPadding(usize),
        /// The reply stream carries a server request for input (an
        /// elicitation) ahead of the result.
        ElicitThenResult,
    }

    /// What the stub server does, beyond answering the protocol.
    #[derive(Clone, Default)]
    pub(crate) struct StubOptions {
        pub call_reply: CallReply,
        /// Answer `reject_status` unless the request carries exactly this
        /// `Authorization` value.
        pub require_authorization: Option<String>,
        /// `401` (the default) or `403`, for a request that fails the check.
        pub reject_status: Option<u16>,
        /// `WWW-Authenticate` lines sent with the rejection; one that names a
        /// resource-metadata URL when empty.
        pub challenge_lines: Vec<String>,
        /// The tool's description is the `Authorization` header the request
        /// carried.
        pub describe_with_authorization: bool,
        /// Answer every request with a `302` to this URL.
        pub redirect_to: Option<String>,
    }

    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        headers: Vec<(String, String)>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Vec<&str> {
            self.headers
                .iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .collect()
        }
    }

    /// A loopback streamable-HTTP MCP server: just enough to be driven by the
    /// production connect path.
    pub(crate) struct Stub {
        port: u16,
        seen: Arc<Mutex<Vec<Seen>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Stub {
        pub(crate) fn start(call_reply: CallReply, require_authorization: Option<&str>) -> Self {
            Self::with(StubOptions {
                call_reply,
                require_authorization: require_authorization.map(str::to_owned),
                ..StubOptions::default()
            })
        }

        pub(crate) fn with(options: StubOptions) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let port = listener.local_addr().expect("addr").port();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let handle = {
                let seen = Arc::clone(&seen);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let _ = stream.set_nonblocking(false);
                                serve_one(stream, &options, &seen);
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                })
            };
            Self {
                port,
                seen,
                stop,
                handle: Some(handle),
            }
        }

        pub(crate) fn url(&self) -> String {
            format!("http://127.0.0.1:{}/mcp", self.port)
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().expect("seen").clone()
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn serve_one(mut stream: std::net::TcpStream, options: &StubOptions, seen: &Mutex<Vec<Seen>>) {
        let call_reply = options.call_reply;
        let required = options.require_authorization.as_deref();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut raw = Vec::new();
        let mut buf = [0u8; 2048];
        let head_end = loop {
            if let Some(at) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break at + 4;
            }
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
        let headers: Vec<(String, String)> = head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        while raw.len() < head_end + length {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
        let body: serde_json::Value =
            serde_json::from_slice(&raw[head_end..]).unwrap_or(serde_json::Value::Null);
        let method = body
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
        seen.lock().expect("seen").push(Seen {
            method: method.clone(),
            headers: headers.clone(),
        });

        let authorization = headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str());
        let reply = |status: &str, extra: &str, content_type: &str, payload: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            )
        };
        let response = if let Some(target) = &options.redirect_to {
            reply(
                "302 Found",
                &format!("Location: {target}\r\n"),
                "text/plain",
                "",
            )
        } else if required.is_some() && authorization != required {
            let status = match options.reject_status {
                Some(403) => "403 Forbidden",
                _ => "401 Unauthorized",
            };
            let lines = if options.challenge_lines.is_empty() {
                vec!["Bearer resource_metadata=\"http://127.0.0.1/meta\"".to_owned()]
            } else {
                options.challenge_lines.clone()
            };
            let extra: String = lines
                .iter()
                .map(|line| format!("WWW-Authenticate: {line}\r\n"))
                .collect();
            reply(status, &extra, "text/plain", "")
        } else if method.starts_with("notifications/") {
            reply("202 Accepted", "", "text/plain", "")
        } else {
            let result = match method.as_str() {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2026-07-28",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stub", "version": "1"},
                }),
                "tools/list" => serde_json::json!({"tools": [{
                    "name": "ping",
                    "description": if options.describe_with_authorization {
                        format!("sent {}", authorization.unwrap_or("nothing"))
                    } else {
                        "pong".to_owned()
                    },
                    "inputSchema": {"type": "object"},
                }]}),
                _ if call_reply == CallReply::EchoAuthorization => serde_json::json!({
                    "content": [{"type": "text", "text": format!(
                        "you sent: {}", authorization.unwrap_or("nothing")
                    )}],
                }),
                _ if matches!(call_reply, CallReply::EchoAfterPadding(_)) => {
                    let CallReply::EchoAfterPadding(pad) = call_reply else {
                        unreachable!("matched above")
                    };
                    serde_json::json!({
                        "content": [{"type": "text", "text": format!(
                            "{}{}", "x".repeat(pad), authorization.unwrap_or("nothing")
                        )}],
                    })
                }
                _ => serde_json::json!({"content": [{"type": "text", "text": "pong"}]}),
            };
            let message = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
            if method == "tools/call" && call_reply == CallReply::ElicitThenResult {
                let ask = serde_json::json!({
                    "jsonrpc": "2.0", "id": "ask-1", "method": "elicitation/create",
                    "params": {"message": "which one?", "requestedSchema": {"type": "object"}},
                });
                let payload = format!("data: {ask}\n\ndata: {message}\n\n");
                reply("200 OK", "", "text/event-stream", &payload)
            } else if method == "tools/call" && call_reply == CallReply::EventStream {
                let progress = serde_json::json!({
                    "jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1},
                });
                let payload = format!("event: message\ndata: {progress}\n\ndata: {message}\n\n");
                reply("200 OK", "", "text/event-stream", &payload)
            } else if method == "initialize" {
                reply(
                    "200 OK",
                    "Mcp-Session-Id: stub-session\r\n",
                    "application/json",
                    &message.to_string(),
                )
            } else {
                reply("200 OK", "", "application/json", &message.to_string())
            }
        };
        let _ = stream.write_all(response.as_bytes());
    }

    fn server(url: &str, headers: &[(&str, &str)], bearer_env: Option<&str>) -> McpServerConfig {
        McpServerConfig {
            name: "remote".to_owned(),
            command: url.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            http: Some(McpHttpEndpoint {
                url: url.to_owned(),
                headers: headers
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                headers_env: Vec::new(),
                bearer_token_env: bearer_env.map(str::to_owned),
            }),
            tools: Default::default(),
        }
    }

    fn env_with(name: &'static str, value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |asked| (asked == name).then(|| value.to_owned())
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn call_ping(
        config: &McpServerConfig,
        lookup: &dyn Fn(&str) -> Option<String>,
        root: Option<&Path>,
    ) -> Result<String, McpConnectError> {
        let connected = connect_mcp_server_with_env(config, root, lookup)?;
        let (mut session, _child) = connected.into_connection();
        let out = session
            .tools_call_with(
                "ping",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut |_method, _params| Err("not expected".to_owned()),
            )
            .map_err(|err| McpConnectError::Handshake(err.to_string()));
        session.close(&CancellationToken::new());
        out.map(|out| out.text)
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rapid-mcp-http-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("scratch");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn tools_call_over_http_returns_a_json_reply_and_an_event_stream_reply() {
        for reply in [CallReply::Json, CallReply::EventStream] {
            let stub = Stub::start(reply, None);
            let text = call_ping(&server(&stub.url(), &[], None), &no_env, None)
                .expect("a tools/call over streamable HTTP returns its result");
            assert_eq!(text, "pong");
            let methods: Vec<String> = stub.seen().into_iter().map(|s| s.method).collect();
            assert_eq!(
                methods,
                [
                    "initialize",
                    "notifications/initialized",
                    "tools/list",
                    "tools/call"
                ],
                "a 202 for the initialized notification is a success"
            );
            for seen in stub.seen() {
                assert_eq!(
                    seen.header("accept"),
                    ["application/json, text/event-stream"],
                    "{} asks for either framing",
                    seen.method
                );
            }
        }
    }

    #[test]
    fn a_bearer_token_from_the_environment_is_sent_once_and_never_printed() {
        let stub = Stub::start(CallReply::Json, Some(&format!("Bearer {SECRET}")));
        let config = server(&stub.url(), &[], Some("MCP_TEST_TOKEN"));
        let text = call_ping(&config, &env_with("MCP_TEST_TOKEN", SECRET), None).expect("call");
        assert_eq!(text, "pong");
        for seen in stub.seen() {
            assert_eq!(
                seen.header("authorization"),
                [format!("Bearer {SECRET}").as_str()],
                "{}: exactly one Authorization header",
                seen.method
            );
        }
        // The entry itself never carries the value.
        assert!(!format!("{config:?}").contains(SECRET));

        // A server that rejects the token: every error text is secret-free.
        let rejecting = Stub::start(CallReply::Json, Some("Bearer something-else"));
        let config = server(&rejecting.url(), &[], Some("MCP_TEST_TOKEN"));
        let err = call_ping(&config, &env_with("MCP_TEST_TOKEN", SECRET), None)
            .expect_err("rejected token");
        let rendered = format!("{err} / {err:?}");
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(rendered.contains("requires authorization"), "{rendered}");
    }

    #[test]
    fn a_missing_bearer_variable_is_a_typed_error_that_names_the_variable() {
        let stub = Stub::start(CallReply::Json, None);
        let config = server(&stub.url(), &[], Some("MCP_TEST_TOKEN"));
        let err = call_ping(&config, &no_env, None).expect_err("variable is unset");
        assert!(matches!(err, McpConnectError::Credential(_)), "{err:?}");
        assert!(err.to_string().contains("MCP_TEST_TOKEN"), "{err}");
        assert!(
            stub.seen().is_empty(),
            "no request is made without the credential"
        );
        let empty = call_ping(&config, &|_| Some(String::new()), None).expect_err("empty");
        assert!(matches!(empty, McpConnectError::Credential(_)));
    }

    #[test]
    fn a_configured_authorization_header_is_sent_verbatim_and_without_one_none_is_invented() {
        let stub = Stub::start(CallReply::Json, Some("Basic dXNlcjpwdw=="));
        let config = server(
            &stub.url(),
            &[("Authorization", "Basic dXNlcjpwdw=="), ("X-Tenant", "t1")],
            None,
        );
        call_ping(&config, &no_env, None).expect("call");
        for seen in stub.seen() {
            assert_eq!(seen.header("authorization"), ["Basic dXNlcjpwdw=="]);
            assert_eq!(seen.header("x-tenant"), ["t1"]);
        }

        let open = Stub::start(CallReply::Json, None);
        call_ping(&server(&open.url(), &[], None), &no_env, None).expect("call");
        for seen in open.seen() {
            assert!(
                seen.header("authorization").is_empty(),
                "{}: no credential configured, none sent",
                seen.method
            );
        }
    }

    #[test]
    fn a_401_is_reported_as_authorization_required_not_as_a_broken_handshake() {
        let stub = Stub::start(CallReply::Json, Some("Bearer needed"));
        let err = call_ping(&server(&stub.url(), &[], None), &no_env, None).expect_err("401");
        let text = err.to_string();
        assert!(text.contains("requires authorization"), "{text}");
        assert!(
            !text.contains("meta"),
            "the challenge is kept for the caller, not echoed into messages: {text}"
        );
    }

    #[test]
    fn every_remote_dial_leaves_an_egress_receipt() {
        let stub = Stub::start(CallReply::Json, None);
        let scratch = Scratch::new();
        call_ping(
            &server(&stub.url(), &[], None),
            &no_env,
            Some(scratch.0.as_path()),
        )
        .expect("call");
        let log = std::fs::read_to_string(scratch.0.join(".rapidlm/egress-receipts.jsonl"))
            .expect("receipt log");
        let lines: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect();
        assert_eq!(
            lines.len(),
            stub.seen().len(),
            "one receipt per POST: {log}"
        );
        for line in &lines {
            assert_eq!(line["tool"], "mcp");
            assert_eq!(line["allowed"], true);
            assert_eq!(
                line["dialled"],
                format!("http://127.0.0.1:{}", stub.port),
                "{line}"
            );
        }
    }

    #[test]
    fn a_project_cannot_read_an_arbitrary_variable_for_a_credential() {
        let stub = Stub::start(CallReply::Json, None);
        let lookup = |name: &str| {
            [
                ("OPENAI_API_KEY", "sk-should-never-leave"),
                ("GITHUB_TOKEN", "ghp-listed-by-the-user"),
                (
                    crate::exec_tools::CREDENTIAL_ENV_ALLOWLIST_VAR,
                    "SOMETHING_ELSE, GITHUB_TOKEN",
                ),
            ]
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
        };
        // The bearer token of a variable the project may not read.
        let err = call_ping(
            &server(&stub.url(), &[], Some("OPENAI_API_KEY")),
            &lookup,
            None,
        )
        .expect_err("refused");
        assert!(matches!(err, McpConnectError::Credential(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("not available to a project-declared server"),
            "{err}"
        );
        assert!(!err.to_string().contains("sk-should-never-leave"), "{err}");
        // And a header value of one.
        let mut config = server(&stub.url(), &[], None);
        config.http.as_mut().expect("remote").headers_env =
            vec![("X-Key".to_owned(), "OPENAI_API_KEY".to_owned())];
        let err = call_ping(&config, &lookup, None).expect_err("refused");
        assert!(err.to_string().contains("headers_env"), "{err}");
        assert!(
            stub.seen().is_empty(),
            "nothing was sent: {:?}",
            stub.seen()
        );
        // A variable the user listed in their own environment is allowed.
        let ok = call_ping(
            &server(&stub.url(), &[], Some("GITHUB_TOKEN")),
            &lookup,
            None,
        );
        assert_eq!(ok.expect("listed"), "pong");
        for seen in stub.seen() {
            assert_eq!(
                seen.header("authorization"),
                ["Bearer ghp-listed-by-the-user"]
            );
        }
    }

    #[test]
    fn a_tool_description_that_carries_a_credential_does_not_reach_the_model() {
        let stub = Stub::with(StubOptions {
            describe_with_authorization: true,
            ..StubOptions::default()
        });
        let config = server(
            &stub.url(),
            &[("Authorization", "Bearer tok-desc-123456")],
            None,
        );
        let connected = connect_mcp_server_with_env(&config, None, &no_env).expect("connects");
        let description = connected.tools[0].description.clone().unwrap_or_default();
        assert!(description.starts_with("sent "), "{description}");
        assert!(!description.contains("tok-desc-123456"), "{description}");
        assert!(description.contains("[redacted]"), "{description}");
    }

    #[test]
    fn a_redirect_off_the_configured_origin_is_refused_and_leaves_a_refusal_receipt() {
        let stub = Stub::with(StubOptions {
            redirect_to: Some("http://example.com/elsewhere".to_owned()),
            ..StubOptions::default()
        });
        let scratch = Scratch::new();
        let err = call_ping(
            &server(&stub.url(), &[], None),
            &no_env,
            Some(scratch.0.as_path()),
        )
        .expect_err("the redirect leaves the scope");
        assert!(err.to_string().contains("outside"), "{err}");
        let log = std::fs::read_to_string(scratch.0.join(".rapidlm/egress-receipts.jsonl"))
            .expect("receipt log");
        let denied: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .filter(|line: &serde_json::Value| line["allowed"] == false)
            .collect();
        assert_eq!(denied.len(), 1, "{log}");
        assert_eq!(denied[0]["tool"], "mcp");
        assert_eq!(denied[0]["reason"], "egress_policy");
        assert!(
            denied[0]["dialled"]
                .as_str()
                .unwrap_or_default()
                .starts_with("http://127.0.0.1:")
        );
    }

    #[test]
    fn every_challenge_line_counts_when_a_403_names_the_scope_on_the_second() {
        let stub = Stub::with(StubOptions {
            require_authorization: Some("Bearer needed".to_owned()),
            reject_status: Some(403),
            challenge_lines: vec![
                "Bearer realm=\"x\"".to_owned(),
                "Bearer error=\"insufficient_scope\", scope=\"admin\"".to_owned(),
            ],
            ..StubOptions::default()
        });
        let err = call_ping(&server(&stub.url(), &[], None), &no_env, None).expect_err("403");
        assert!(
            err.to_string().contains("broader scope"),
            "the second WWW-Authenticate line is read: {err}"
        );
    }

    #[test]
    fn a_remote_gate_built_for_a_configured_name_refuses_private_answers() {
        use llm_router::providers::dial::{DialGate, DialTarget};
        let http = McpHttpEndpoint {
            url: "https://mcp.example.com/mcp".to_owned(),
            headers: Vec::new(),
            headers_env: Vec::new(),
            bearer_token_env: None,
        };
        let (io, _secrets) =
            crate::exec_tools::http_io_for(&http, true, "mcp.example.com", 443, None, &no_env)
                .expect("io");
        let gate = io.gate.expect("a gate is attached");
        let target = DialTarget {
            https: true,
            host: "mcp.example.com",
            port: 443,
        };
        let private: Vec<std::net::SocketAddr> = vec!["10.0.0.5:443".parse().expect("addr")];
        assert!(gate.permit(target, None, &private).is_err());
        assert!(!gate.receipts()[0].allowed);
    }
}
