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
    // An accepted socket can inherit the listener's non-blocking mode.
    let _ = stream.set_nonblocking(false);
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

// --- an MCP server and an authorization server, for the whole flow -------------------

use std::collections::HashMap;

use crate::mcp_oauth::challenge_of;
use crate::mcp_oauth_listen::{OpenError, Opener};

/// What the world does besides the usual thing; every field has the answer a
/// well-behaved server gives.
#[derive(Clone)]
pub(crate) struct WorldOptions {
    pub registration: bool,
    pub pkce_methods: Vec<String>,
    pub issuer_in_metadata: Option<String>,
    pub iss_in_redirect: Option<String>,
    pub token_type: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<serde_json::Value>,
    pub iss_in_token_reply: Option<String>,
    pub token_status: u16,
    pub token_error: Option<String>,
    pub deny: bool,
    pub client_secret: Option<String>,
    pub challenge_scope: Option<String>,
    pub resource_metadata_in_challenge: bool,
    pub prm_resource: Option<String>,
    pub prm_status: u16,
    pub as_status: u16,
    pub authorization_servers: Option<Vec<String>>,
    pub preregistered_client: Option<String>,
    /// The resource metadata is also served here, and the challenge names it.
    pub prm_alias: Option<String>,
    /// The challenge names this address for the resource metadata instead.
    pub challenge_metadata_url: Option<String>,
    /// The alias serves this text instead of the resource document.
    pub prm_alias_body: Option<String>,
    /// A GET of the MCP address is answered with a redirect to this address.
    pub mcp_get_redirect: Option<String>,
    /// The metadata names this token endpoint instead of its own.
    pub token_endpoint: Option<String>,
    /// A refresh grant gives a new refresh token too (the old one is spent).
    pub refresh_rotates: bool,
    /// A refresh grant is answered with this error (status 400).
    pub refresh_error: Option<String>,
    /// A refresh grant takes this long to answer.
    pub refresh_delay: std::time::Duration,
    /// The lifetime, in seconds, a refresh grant gives its token.
    pub refresh_expires_in: u64,
    /// A refresh makes the token it replaces stop working at the server.
    pub refresh_retires_old: bool,
    /// The token a refresh gives is one the MCP server does not take.
    pub refresh_issues_unusable: bool,
    /// The status the revocation endpoint answers with.
    pub revoke_status: u16,
    /// A tool call answers with the `Authorization` header it carried.
    pub echo_authorization: bool,
    /// The status of a registration reply (`201` registers).
    pub registration_status: u16,
    /// The `token_endpoint_auth_method` a registration reply names.
    pub registration_method: String,
    /// The resource a code was issued for, as this authorization server
    /// expects it (its own MCP server's address when absent): set on an
    /// authorization server that stands in for another server's.
    pub expected_resource: Option<String>,
}

impl Default for WorldOptions {
    fn default() -> Self {
        Self {
            registration: true,
            pkce_methods: vec!["S256".to_owned()],
            issuer_in_metadata: None,
            iss_in_redirect: None,
            token_type: "Bearer".to_owned(),
            access_token: "world-access-canary-1".to_owned(),
            refresh_token: Some("world-refresh-canary-1".to_owned()),
            expires_in: Some(serde_json::json!(3600)),
            iss_in_token_reply: None,
            token_status: 200,
            token_error: None,
            deny: false,
            client_secret: None,
            challenge_scope: Some("read".to_owned()),
            resource_metadata_in_challenge: true,
            prm_resource: None,
            prm_status: 200,
            as_status: 200,
            authorization_servers: None,
            preregistered_client: Some("pre-client".to_owned()),
            prm_alias: None,
            challenge_metadata_url: None,
            prm_alias_body: None,
            mcp_get_redirect: None,
            token_endpoint: None,
            refresh_rotates: false,
            refresh_error: None,
            refresh_delay: std::time::Duration::ZERO,
            refresh_expires_in: 3600,
            refresh_retires_old: true,
            refresh_issues_unusable: false,
            revoke_status: 200,
            echo_authorization: false,
            registration_status: 201,
            registration_method: "none".to_owned(),
            expected_resource: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Issued {
    pub client_id: String,
    pub challenge: String,
    pub redirect_uri: String,
    pub resource: String,
    pub scope: String,
}

#[derive(Default)]
pub(crate) struct WorldState {
    pub registered: Vec<(String, serde_json::Value)>,
    pub codes: HashMap<String, Issued>,
    pub authorize_queries: Vec<Vec<(String, String)>>,
    pub token_forms: Vec<Vec<(String, String)>>,
    pub token_authorizations: Vec<Option<String>>,
    pub revokes: Vec<Vec<(String, String)>>,
    pub verifier_ok: Vec<bool>,
    /// The access tokens the MCP server takes now (empty: the configured one).
    pub valid_access: Vec<String>,
    /// The refresh tokens the authorization server takes now (empty: the
    /// configured one).
    pub valid_refresh: Vec<String>,
    /// The forms of the refresh grants it was asked for, and their
    /// `Authorization` headers.
    pub refresh_forms: Vec<Vec<(String, String)>>,
    pub refresh_authorizations: Vec<Option<String>>,
    next: u32,
}

pub(crate) struct World {
    pub mcp: HttpStub,
    pub auth: Option<HttpStub>,
    pub state: Arc<Mutex<WorldState>>,
    pub options: Arc<Mutex<WorldOptions>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Both,
    Mcp,
    Authorization,
}

impl World {
    /// The MCP server and the authorization server on one origin.
    pub fn start(options: WorldOptions) -> Self {
        Self::build(options, false)
    }

    /// Each on an origin of its own.
    pub fn split(options: WorldOptions) -> Self {
        Self::build(options, true)
    }

    fn build(options: WorldOptions, split: bool) -> Self {
        let state = Arc::new(Mutex::new(WorldState::default()));
        let options = Arc::new(Mutex::new(options));
        let bases = Arc::new(Mutex::new((String::new(), String::new())));
        let make = |role: Role| {
            let (state, options, bases) = (state.clone(), options.clone(), bases.clone());
            HttpStub::start_with_self(move |request, me| {
                let (mcp_base, as_base) = bases.lock().unwrap().clone();
                let mcp_base = if mcp_base.is_empty() {
                    me.to_owned()
                } else {
                    mcp_base
                };
                let as_base = if as_base.is_empty() {
                    me.to_owned()
                } else {
                    as_base
                };
                world_handler(request, role, &mcp_base, &as_base, &state, &options)
            })
        };
        let (mcp, auth) = if split {
            (make(Role::Mcp), Some(make(Role::Authorization)))
        } else {
            (make(Role::Both), None)
        };
        {
            let mut bases = bases.lock().unwrap();
            bases.0 = mcp.base();
            bases.1 = auth.as_ref().map_or_else(|| mcp.base(), HttpStub::base);
        }
        Self {
            mcp,
            auth,
            state,
            options,
        }
    }

    pub fn mcp_url(&self) -> String {
        self.mcp.url("/mcp")
    }

    pub fn issuer(&self) -> String {
        self.auth
            .as_ref()
            .map_or_else(|| self.mcp.base(), HttpStub::base)
    }

    /// Every request either server got, with the stub's own order kept.
    pub fn requests(&self) -> Vec<Request> {
        let mut all = self.mcp.requests();
        if let Some(auth) = &self.auth {
            all.extend(auth.requests());
        }
        all
    }

    /// The `host:port` of every server that got a request.
    pub fn authorities_seen(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for stub in std::iter::once(&self.mcp).chain(self.auth.iter()) {
            if !stub.requests().is_empty() {
                seen.push(stub.authority());
            }
        }
        seen
    }

    pub fn set(&self, change: impl FnOnce(&mut WorldOptions)) {
        change(&mut self.options.lock().unwrap());
    }

    pub fn token_requests(&self) -> usize {
        self.state.lock().unwrap().token_forms.len()
    }
}

fn world_handler(
    request: &Request,
    role: Role,
    mcp_base: &str,
    as_base: &str,
    state: &Mutex<WorldState>,
    options: &Mutex<WorldOptions>,
) -> Response {
    let o = options.lock().unwrap().clone();
    let route = request.route();
    let serves = |wanted: Role| role == Role::Both || role == wanted;
    match route {
        "/mcp" if serves(Role::Mcp) && request.method == "GET" && o.mcp_get_redirect.is_some() => {
            Response::redirect(302, o.mcp_get_redirect.as_deref().unwrap_or_default())
        }
        "/mcp" if serves(Role::Mcp) => mcp_endpoint(request, mcp_base, &o, state),
        "/.well-known/oauth-protected-resource/mcp" | "/.well-known/oauth-protected-resource"
            if serves(Role::Mcp) =>
        {
            prm(&o, mcp_base, as_base)
        }
        alias if serves(Role::Mcp) && o.prm_alias.as_deref() == Some(alias) => {
            match &o.prm_alias_body {
                Some(page) => Response::text(200, "text/html", page),
                None => prm(&o, mcp_base, as_base),
            }
        }
        "/.well-known/oauth-authorization-server" if serves(Role::Authorization) => {
            if o.as_status != 200 {
                return Response::json(o.as_status, "{}");
            }
            let mut doc = serde_json::json!({
                "issuer": o.issuer_in_metadata.clone().unwrap_or_else(|| as_base.to_owned()),
                "authorization_endpoint": format!("{as_base}/authorize"),
                "token_endpoint": o.token_endpoint.clone().unwrap_or_else(|| format!("{as_base}/token")),
                "revocation_endpoint": format!("{as_base}/revoke"),
                "code_challenge_methods_supported": o.pkce_methods,
                "token_endpoint_auth_methods_supported": ["none", "client_secret_basic"],
            });
            if o.registration {
                doc["registration_endpoint"] = format!("{as_base}/register").into();
            }
            Response::json(200, &doc.to_string())
        }
        "/register" if serves(Role::Authorization) && o.registration => {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null);
            let mut state = state.lock().unwrap();
            if o.registration_status != 201 {
                return Response::json(
                    o.registration_status,
                    r#"{"error":"invalid_client_metadata"}"#,
                );
            }
            state.next += 1;
            let client_id = format!("dyn-client-{}", state.next);
            state.registered.push((client_id.clone(), body));
            Response::json(
                201,
                &serde_json::json!({
                    "client_id": client_id,
                    "client_secret": "must-never-be-stored-canary",
                    "token_endpoint_auth_method": o.registration_method,
                })
                .to_string(),
            )
        }
        "/authorize" if serves(Role::Authorization) => authorize(request, as_base, state, &o),
        "/token" if serves(Role::Authorization) => token(request, mcp_base, state, &o),
        "/revoke" if serves(Role::Authorization) => {
            state.lock().unwrap().revokes.push(request.form());
            Response::json(o.revoke_status, "{}")
        }
        _ => Response::not_found(),
    }
}

fn prm(o: &WorldOptions, mcp_base: &str, as_base: &str) -> Response {
    if o.prm_status != 200 {
        return Response::json(o.prm_status, "{}");
    }
    let servers = o
        .authorization_servers
        .clone()
        .unwrap_or_else(|| vec![as_base.to_owned()]);
    Response::json(
        200,
        &serde_json::json!({
            "resource": o.prm_resource.clone().unwrap_or_else(|| format!("{mcp_base}/mcp")),
            "authorization_servers": servers,
            "scopes_supported": ["read", "write"],
        })
        .to_string(),
    )
}

fn mcp_endpoint(
    request: &Request,
    mcp_base: &str,
    o: &WorldOptions,
    state: &Mutex<WorldState>,
) -> Response {
    let authorized = {
        let state = state.lock().unwrap();
        let presented = request.header("authorization");
        if state.valid_access.is_empty() {
            presented == Some(&format!("Bearer {}", o.access_token))
        } else {
            state
                .valid_access
                .iter()
                .any(|token| presented == Some(&format!("Bearer {token}")))
        }
    };
    if !authorized {
        let mut challenge = String::from("Bearer");
        let mut parts = Vec::new();
        if let Some(url) = &o.challenge_metadata_url {
            parts.push(format!("resource_metadata=\"{url}\""));
        } else if let Some(alias) = &o.prm_alias {
            parts.push(format!("resource_metadata=\"{mcp_base}{alias}\""));
        } else if o.resource_metadata_in_challenge {
            parts.push(format!(
                "resource_metadata=\"{mcp_base}/.well-known/oauth-protected-resource/mcp\""
            ));
        }
        if let Some(scope) = &o.challenge_scope {
            parts.push(format!("scope=\"{scope}\""));
        }
        if !parts.is_empty() {
            challenge.push(' ');
            challenge.push_str(&parts.join(", "));
        }
        return Response::text(401, "text/plain", "").with_header("WWW-Authenticate", &challenge);
    }
    if request.method != "POST" {
        return Response::text(405, "text/plain", "");
    }
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null);
    let method = body["method"].as_str().unwrap_or_default();
    let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
    if method.starts_with("notifications/") {
        return Response::text(202, "text/plain", "");
    }
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2026-07-28",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "world", "version": "1"},
        }),
        "tools/list" => serde_json::json!({"tools": [{
            "name": "ping", "description": "pong", "inputSchema": {"type": "object"},
        }]}),
        _ if o.echo_authorization => {
            serde_json::json!({"content": [{"type": "text", "text": format!(
                "you sent: {}",
                request.header("authorization").unwrap_or("nothing")
            )}]})
        }
        _ => serde_json::json!({"content": [{"type": "text", "text": "pong"}]}),
    };
    let reply = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
    let response = Response::json(200, &reply.to_string());
    if method == "initialize" {
        response.with_header("Mcp-Session-Id", "world-session")
    } else {
        response
    }
}

fn authorize(
    request: &Request,
    as_base: &str,
    state: &Mutex<WorldState>,
    o: &WorldOptions,
) -> Response {
    let query = request.query();
    let get = |name: &str| {
        query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    state.lock().unwrap().authorize_queries.push(query.clone());
    let redirect = get("redirect_uri");
    let client = get("client_id");
    let known = {
        let state = state.lock().unwrap();
        state.registered.iter().any(|(id, _)| *id == client)
            || o.preregistered_client.as_deref() == Some(client.as_str())
    };
    if !known || get("response_type") != "code" || get("code_challenge_method") != "S256" {
        return Response::json(400, r#"{"error":"invalid_request"}"#);
    }
    let sep = if redirect.contains('?') { '&' } else { '?' };
    if o.deny {
        return Response::redirect(
            302,
            &format!(
                "{redirect}{sep}error=access_denied&state={}",
                crate::mcp_oauth::encode_component(&get("state"))
            ),
        );
    }
    let code = {
        let mut state = state.lock().unwrap();
        state.next += 1;
        let code = format!("world-code-canary-{}", state.next);
        state.codes.insert(
            code.clone(),
            Issued {
                client_id: client,
                challenge: get("code_challenge"),
                redirect_uri: redirect.clone(),
                resource: get("resource"),
                scope: get("scope"),
            },
        );
        code
    };
    let mut location = format!(
        "{redirect}{sep}code={code}&state={}",
        crate::mcp_oauth::encode_component(&get("state"))
    );
    if let Some(iss) = &o.iss_in_redirect {
        location.push_str(&format!("&iss={}", crate::mcp_oauth::encode_component(iss)));
    }
    let _ = as_base;
    Response::redirect(302, &location)
}

fn token(
    request: &Request,
    mcp_base: &str,
    state: &Mutex<WorldState>,
    o: &WorldOptions,
) -> Response {
    let form = request.form();
    let get = |name: &str| {
        form.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    {
        let mut state = state.lock().unwrap();
        state.token_forms.push(form.clone());
        state
            .token_authorizations
            .push(request.header("authorization").map(str::to_owned));
    }
    if get("grant_type") == "refresh_token" {
        return refresh_grant(request, &form, state, o);
    }
    if let Some(error) = &o.token_error {
        return Response::json(
            o.token_status.max(400),
            &serde_json::json!({"error": error, "error_description": "secret-canary <b>"})
                .to_string(),
        );
    }
    if let Some(secret) = &o.client_secret {
        use base64::Engine as _;
        let basic = request
            .header("authorization")
            .and_then(|h| h.strip_prefix("Basic "));
        let from_header = basic
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .and_then(|raw| String::from_utf8(raw).ok());
        let expected = format!(
            "{}:{}",
            crate::mcp_oauth::encode_component(&get("client_id")),
            crate::mcp_oauth::encode_component(secret)
        );
        if from_header.as_deref() != Some(expected.as_str()) && get("client_secret") != *secret {
            return Response::json(401, r#"{"error":"invalid_client"}"#);
        }
    }
    let issued = state.lock().unwrap().codes.remove(&get("code"));
    let Some(issued) = issued else {
        return Response::json(400, r#"{"error":"invalid_grant"}"#);
    };
    let verifier_ok = challenge_of(&get("code_verifier")) == issued.challenge
        && get("redirect_uri") == issued.redirect_uri
        && get("client_id") == issued.client_id
        && get("resource") == issued.resource
        && issued.resource
            == o.expected_resource
                .clone()
                .unwrap_or_else(|| format!("{mcp_base}/mcp"));
    state.lock().unwrap().verifier_ok.push(verifier_ok);
    if !verifier_ok {
        return Response::json(400, r#"{"error":"invalid_grant"}"#);
    }
    let mut reply = serde_json::json!({
        "access_token": o.access_token,
        "token_type": o.token_type,
    });
    if let Some(refresh) = &o.refresh_token {
        reply["refresh_token"] = refresh.clone().into();
    }
    if let Some(expires) = &o.expires_in {
        reply["expires_in"] = expires.clone();
    }
    if let Some(iss) = &o.iss_in_token_reply {
        reply["iss"] = iss.clone().into();
    }
    if !issued.scope.is_empty() {
        reply["scope"] = issued.scope.into();
    }
    Response::json(200, &reply.to_string())
}

fn refresh_grant(
    request: &Request,
    form: &[(String, String)],
    state: &Mutex<WorldState>,
    o: &WorldOptions,
) -> Response {
    let get = |name: &str| {
        form.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    {
        let mut state = state.lock().unwrap();
        state.refresh_forms.push(form.to_vec());
        state
            .refresh_authorizations
            .push(request.header("authorization").map(str::to_owned));
    }
    if !o.refresh_delay.is_zero() {
        std::thread::sleep(o.refresh_delay);
    }
    if let Some(error) = &o.refresh_error {
        return Response::json(400, &serde_json::json!({"error": error}).to_string());
    }
    let mut state = state.lock().unwrap();
    if state.valid_refresh.is_empty() {
        state.valid_refresh = o.refresh_token.iter().cloned().collect();
    }
    if !state
        .valid_refresh
        .iter()
        .any(|t| *t == get("refresh_token"))
    {
        return Response::json(400, r#"{"error":"invalid_grant"}"#);
    }
    state.next += 1;
    let n = state.next;
    let access = format!("world-access-r{n}");
    if state.valid_access.is_empty() {
        state.valid_access = vec![o.access_token.clone()];
    }
    if o.refresh_retires_old {
        state.valid_access.clear();
    }
    if !o.refresh_issues_unusable {
        state.valid_access.push(access.clone());
    }
    let mut reply = serde_json::json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": o.refresh_expires_in,
    });
    if o.refresh_rotates {
        let fresh = format!("world-refresh-r{n}");
        let spent = get("refresh_token");
        state.valid_refresh.retain(|t| *t != spent);
        state.valid_refresh.push(fresh.clone());
        reply["refresh_token"] = fresh.into();
    }
    Response::json(200, &reply.to_string())
}

// --- the browser's far end -----------------------------------------------------------

/// A GET that follows nothing: the status, and the `Location` if there was one.
pub(crate) fn http_get(url: &str) -> Option<(u16, Option<String>)> {
    let parsed = crate::mcp_oauth::Url::parse(url).ok()?;
    let mut stream = std::net::TcpStream::connect((parsed.host(), parsed.port())).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        parsed.target(),
        parsed.authority()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    let status = raw.split(' ').nth(1)?.parse::<u16>().ok()?;
    let location = raw
        .lines()
        .find_map(|line| {
            line.strip_prefix("Location: ")
                .or_else(|| line.strip_prefix("location: "))
        })
        .map(|text| text.trim().to_owned());
    Some((status, location))
}

/// What a fake browser does with the address it is given.
#[derive(Clone)]
pub(crate) enum Browse {
    /// Visit the authorization address, then follow its redirect.
    Follow,
    /// Visit it and follow the redirect after changing it.
    FollowChanged(fn(&str) -> String),
    /// Open and do nothing more.
    Idle,
    /// Cannot be opened.
    Unavailable,
}

pub(crate) struct FakeBrowser {
    pub behavior: Browse,
    pub opened: Arc<Mutex<Vec<String>>>,
}

impl FakeBrowser {
    pub fn new(behavior: Browse) -> Self {
        Self {
            behavior,
            opened: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Opener for FakeBrowser {
    fn open(&self, url: &str) -> Result<(), OpenError> {
        self.opened.lock().unwrap().push(url.to_owned());
        let change = match &self.behavior {
            Browse::Unavailable => {
                return Err(OpenError::Unavailable("no opener in this test".to_owned()));
            }
            Browse::Idle => return Ok(()),
            Browse::Follow => None,
            Browse::FollowChanged(change) => Some(*change),
        };
        let url = url.to_owned();
        std::thread::spawn(move || {
            if let Some((302, Some(location))) = http_get(&url) {
                let location = change.map_or(location.clone(), |change| change(&location));
                let _ = http_get(&location);
            }
        });
        Ok(())
    }
}
