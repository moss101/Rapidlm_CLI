//! `rapid mcp auth <server>` (EXT-02-2): authorize RapidLM against a remote MCP
//! server that asked for it.
//!
//! Obtaining a credential is a privileged act, so this is reached only from
//! argv (and a slash command the person types): no tool, hook or goal path
//! calls it. The steps are the specification's, in its order, and each can end
//! the command with a typed [`Reason`] and nothing stored:
//!
//! 1. **Gate** — the entry may be authorized (no credential of its own, not
//!    `auth: false`, `https` or this machine), the managed policy allows it,
//!    the credential store is there. All before any network contact.
//! 2. **Discover** — the server's protected-resource metadata, then the
//!    authorization server's. Read-only, `https` (the loopback exception for a
//!    server that is itself on this machine), bounded, no credential sent.
//! 3. **Plan** — what will be contacted and asked, shown before anything else
//!    happens; the person confirms, or `--yes` does. `--dry-run` stops here.
//! 4. **Client** — the entry's pre-registered client, one registered earlier,
//!    or a public client registered now; else `no_client`.
//! 5. **Authorize** — PKCE `S256`, a 256-bit `state`, the server's canonical
//!    URL as the `resource`; the browser is opened without a shell, or the
//!    address is printed; a loopback listener takes the one valid redirect.
//! 6. **Exchange and store** — the code for tokens, validated; the tokens into
//!    the credential store and the non-secret metadata beside them.
//! 7. **Record** — an audit line, never with a token in it.
//!
//! Tokens, the code and the verifier exist only in this module's locals and the
//! credential store: they are never in an error text, a plan, a log or a file.

use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::exec_tools::{McpAuthMode, McpAuthSettings, McpHttpEndpoint};
use crate::mcp_auth::{FileLock, LockError, canonical_url};
use crate::mcp_credentials::{
    Authorization, CredentialError, OauthStore, credential_key, now_ms, put_tokens,
};
use crate::mcp_oauth::{
    AuthorizationRequest, AuthorizationServer, Callback, ExchangeForm, Pkce, ProtocolError, Secret,
    Url, UrlError, authorization_url, basic_authorization, check_endpoint, check_iss,
    exchange_body, new_state, parse_authorization_server, parse_protected_resource,
    parse_registration, parse_token_reply, registration_body, shown,
};
use crate::mcp_oauth_listen::{ListenError, Listener, OpenError, Opener, Waited};
use crate::mcp_oauth_net::{NetError, OauthHttp};

/// How long the redirect is waited for unless `--timeout` says otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The longest `--timeout`.
pub const MAX_TIMEOUT_SECS: u64 = 3600;

/// How long a second authorization waits for the first.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// What the command was asked to do.
#[derive(Clone, Debug)]
pub struct Options {
    /// Print the address and take the redirect from a pasted line, instead of
    /// opening a browser.
    pub no_browser: bool,
    /// Discover and show the plan; change nothing.
    pub dry_run: bool,
    /// How long the redirect is waited for.
    pub timeout: Duration,
    /// Do not ask for confirmation.
    pub yes: bool,
    /// How long a second authorization waits for the first.
    pub lock_wait: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            no_browser: false,
            dry_run: false,
            timeout: DEFAULT_TIMEOUT,
            yes: false,
            lock_wait: LOCK_WAIT,
        }
    }
}

/// What the command runs against.
pub struct Context<'a> {
    /// The server's name in the settings file.
    pub name: &'a str,
    pub http: &'a McpHttpEndpoint,
    pub home: &'a Path,
    /// The environment the command sees (a pre-registered client's secret is
    /// read from here).
    pub env: &'a [(String, String)],
    /// A person is at a terminal.
    pub interactive: bool,
    /// Agent mode (`--non-interactive`).
    pub agent: bool,
    /// The managed policy allows authorization.
    pub allow_authorization: bool,
}

/// Why the command stopped. Every one but a success leaves nothing stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The entry is not one this command applies to.
    NotApplicable,
    /// Agent mode without `--yes` and a pre-registered client.
    AgentMode,
    /// No terminal and no `--yes`.
    ConfirmationRequired,
    Policy,
    NoSecureStore,
    /// Another authorization holds the lock.
    Busy,
    DiscoveryFailed,
    NoClient,
    PortInUse,
    /// The person answered no to the plan.
    Declined,
    DeniedByUser,
    Timeout,
    /// Five requests that were not the redirect.
    Aborted,
    Cancelled,
    ExchangeFailed,
    IssuerChanged,
    Internal,
}

impl Reason {
    /// The word an audit record and a report carry.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::AgentMode => "agent_mode",
            Self::ConfirmationRequired => "confirmation_required",
            Self::Policy => "policy",
            Self::NoSecureStore => "no_secure_store",
            Self::Busy => "busy",
            Self::DiscoveryFailed => "discovery_failed",
            Self::NoClient => "no_client",
            Self::PortInUse => "port_in_use",
            Self::Declined => "declined",
            Self::DeniedByUser => "denied_by_user",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
            Self::Cancelled => "cancelled",
            Self::ExchangeFailed => "exchange_failed",
            Self::IssuerChanged => "issuer_changed",
            Self::Internal => "internal",
        }
    }

    /// Whether the stop is an event worth an audit record: one that happened
    /// after the command was allowed to start.
    pub fn audited(self) -> bool {
        matches!(
            self,
            Self::Policy
                | Self::NoSecureStore
                | Self::DiscoveryFailed
                | Self::NoClient
                | Self::DeniedByUser
                | Self::Timeout
                | Self::Aborted
                | Self::ExchangeFailed
                | Self::IssuerChanged
        )
    }
}

/// A stop: why, and what to tell the person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub reason: Reason,
    pub message: String,
    /// The issuer, once discovery found it.
    pub issuer: Option<String>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn fail(reason: Reason, message: impl Into<String>) -> Failure {
    Failure {
        reason,
        message: message.into(),
        issuer: None,
    }
}

/// What happened, for the audit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditEvent {
    Authorized {
        server: String,
        origin: String,
        issuer: String,
        client_id: String,
        scopes: Vec<String>,
        expires_at_ms: Option<i64>,
    },
    Failed {
        server: String,
        reason: &'static str,
        issuer: Option<String>,
    },
    /// `rapid mcp logout`: the tokens were deleted (and revoked, if the
    /// authorization server took the request).
    Deauthorized {
        server: String,
        origin: String,
        issuer: String,
        client_id: String,
        revocation: &'static str,
        client_forgotten: bool,
    },
}

/// Where audit records go.
pub trait Audit {
    fn record(&self, event: &AuditEvent);
}

/// Appends audit lines to `mcp-audit.jsonl` in the RapidLM home: owner-only,
/// one JSON object a line, started over past a megabyte. No token, code,
/// verifier or secret is ever a field.
pub struct FileAudit {
    path: std::path::PathBuf,
}

/// The audit file's name.
pub const AUDIT_FILE: &str = "mcp-audit.jsonl";

const MAX_AUDIT_BYTES: u64 = 1024 * 1024;

impl FileAudit {
    pub fn in_home(home: &Path) -> Self {
        Self {
            path: home.join(AUDIT_FILE),
        }
    }

    /// The line an event is recorded as.
    pub fn line(event: &AuditEvent) -> String {
        let at = crate::headless::jsonl::now_rfc3339();
        match event {
            AuditEvent::Authorized {
                server,
                origin,
                issuer,
                client_id,
                scopes,
                expires_at_ms,
            } => serde_json::json!({
                "at": at, "event": "mcp.authorized", "server": server, "origin": origin,
                "issuer": issuer, "client_id": client_id, "scopes": scopes,
                "expires_at_ms": expires_at_ms,
            }),
            AuditEvent::Failed {
                server,
                reason,
                issuer,
            } => serde_json::json!({
                "at": at, "event": "mcp.auth_failed", "server": server, "reason": reason,
                "issuer": issuer,
            }),
            AuditEvent::Deauthorized {
                server,
                origin,
                issuer,
                client_id,
                revocation,
                client_forgotten,
            } => serde_json::json!({
                "at": at, "event": "mcp.deauthorized", "server": server, "origin": origin,
                "issuer": issuer, "client_id": client_id, "revocation": revocation,
                "client_forgotten": client_forgotten,
            }),
        }
        .to_string()
    }
}

impl Audit for FileAudit {
    fn record(&self, event: &AuditEvent) {
        use std::io::Write as _;
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::metadata(&self.path).is_ok_and(|meta| meta.len() > MAX_AUDIT_BYTES) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("jsonl.1"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(&self.path) {
            let _ = writeln!(file, "{}", Self::line(event));
        }
    }
}

/// There is nobody to answer (no terminal, or the input ended).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoAnswer;

/// How the command talks to the person.
pub trait Interaction {
    /// A line for the person.
    fn say(&mut self, text: &str);

    /// Ask a yes-or-no question; `Err` when there is no answer to be had.
    fn confirm(&mut self, question: &str) -> Result<bool, NoAnswer>;

    /// Start taking lines the person types or pastes (the redirect's address
    /// when no browser is used); `None` when nothing can be read.
    fn start_paste(&mut self) -> Option<Receiver<String>>;
}

/// The command line's side of [`Interaction`]: lines to standard output, the
/// answer and any pasted address from standard input.
#[derive(Default)]
pub struct StdioInteraction;

impl Interaction for StdioInteraction {
    fn say(&mut self, text: &str) {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }

    fn confirm(&mut self, question: &str) -> Result<bool, NoAnswer> {
        use std::io::Write as _;
        {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{question}");
            let _ = out.flush();
        }
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => Err(NoAnswer),
            Ok(_) => Ok(matches!(
                line.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            )),
        }
    }

    fn start_paste(&mut self) -> Option<Receiver<String>> {
        let (send, receive) = std::sync::mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("mcp-auth-paste".to_owned())
            .spawn(move || {
                let mut line = String::new();
                while std::io::stdin()
                    .read_line(&mut line)
                    .is_ok_and(|read| read > 0)
                {
                    if send.send(line.trim().to_owned()).is_err() {
                        return;
                    }
                    line.clear();
                }
            })
            .ok()?;
        Some(receive)
    }
}

/// What the plan says will happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub name: String,
    pub server: String,
    pub issuer: String,
    /// Hosts already contacted to learn this (the server, the authorization
    /// server).
    pub discovered_from: Vec<String>,
    pub registration_host: Option<String>,
    pub authorization_host: String,
    pub token_host: String,
    pub revocation_host: Option<String>,
    pub scopes: Vec<String>,
    pub client: ClientPlan,
    pub redirect: String,
}

/// Which client the request is made as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientPlan {
    PreRegistered {
        id: String,
        secret_env: Option<String>,
    },
    Reused {
        id: String,
    },
    Register,
}

impl Plan {
    /// Every host the flow touches, once each.
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts = self.discovered_from.clone();
        for host in [
            self.registration_host.clone(),
            Some(self.authorization_host.clone()),
            Some(self.token_host.clone()),
            self.revocation_host.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        hosts
    }

    /// The plan, as the lines the person reads.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("authorize {}", shown(&self.name)),
            format!("  server      {}", self.server),
            format!("  issuer      {}", self.issuer),
            format!("  contacts    {}", self.hosts().join(", ")),
        ];
        let mut purposes = vec![format!(
            "discovery {} (done)",
            self.discovered_from.join(", ")
        )];
        if let Some(host) = &self.registration_host
            && self.client == ClientPlan::Register
        {
            purposes.push(format!("registration {host}"));
        }
        purposes.push(format!(
            "authorization {} (your browser)",
            self.authorization_host
        ));
        purposes.push(format!("token {}", self.token_host));
        if let Some(host) = &self.revocation_host {
            purposes.push(format!("revocation {host} (at logout)"));
        }
        lines.push(format!("  for         {}", purposes.join("; ")));
        lines.push(format!(
            "  scopes      {}",
            if self.scopes.is_empty() {
                "(none requested)".to_owned()
            } else {
                self.scopes.join(" ")
            }
        ));
        lines.push(match &self.client {
            ClientPlan::PreRegistered { id, secret_env } => format!(
                "  client      pre-registered `{}`{}",
                shown(id),
                secret_env
                    .as_ref()
                    .map_or(String::new(), |variable| format!(
                        ", secret from ${variable} sent to {}",
                        self.token_host
                    ))
            ),
            ClientPlan::Reused { id } => {
                format!("  client      registered earlier `{}`", shown(id))
            }
            ClientPlan::Register => format!(
                "  client      a public client (no secret) will be registered at {}",
                self.registration_host.as_deref().unwrap_or("?")
            ),
        });
        lines.push(format!(
            "  redirect    {} (this machine only)",
            self.redirect
        ));
        lines
    }
}

/// What a successful run reports. No token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedInfo {
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub expires_in_secs: Option<u64>,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Authorized(AuthorizedInfo),
    /// Discovery only: nothing was registered, opened or stored.
    DryRun(Plan),
}

/// The pieces a run uses that are not the entry.
pub struct Services<'a> {
    pub net: &'a dyn OauthHttp,
    pub io: &'a mut dyn Interaction,
    pub opener: &'a dyn Opener,
    pub audit: &'a dyn Audit,
    pub cancelled: &'a dyn Fn() -> bool,
    /// Where receipts and the project-scoped files are; `None` in a test that
    /// does not look.
    pub oauth_store: &'a OauthStore,
    pub state_store: &'a crate::mcp_auth::AuthStateStore,
}

/// Authorize `ctx.name`.
pub fn run(
    ctx: &Context<'_>,
    options: &Options,
    services: &mut Services<'_>,
) -> Result<Outcome, Failure> {
    let result = run_steps(ctx, options, services);
    match &result {
        Ok(Outcome::Authorized(info)) => {
            let origin = crate::mcp_auth::origin_of(&canonical_url(&ctx.http.url));
            services.audit.record(&AuditEvent::Authorized {
                server: ctx.name.to_owned(),
                origin,
                issuer: info.issuer.clone(),
                client_id: info.client_id.clone(),
                scopes: info.scopes.clone(),
                expires_at_ms: info
                    .expires_in_secs
                    .map(|secs| now_ms().saturating_add(i64::try_from(secs).unwrap_or(0) * 1000)),
            });
        }
        Err(failure) if failure.reason.audited() => {
            services.audit.record(&AuditEvent::Failed {
                server: ctx.name.to_owned(),
                reason: failure.reason.code(),
                issuer: failure.issuer.clone(),
            });
        }
        _ => {}
    }
    result
}

fn protocol(reason: Reason, err: &ProtocolError) -> Failure {
    fail(reason, err.to_string())
}

/// The environment variable lookup the credential rules use.
fn lookup<'a>(env: &'a [(String, String)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        env.iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

/// The body of a 200 reply, or why there is none.
fn fetch(net: &dyn OauthHttp, url: &Url) -> Result<Vec<u8>, String> {
    match net.get(url) {
        Ok(reply) if reply.status == 200 => Ok(reply.body),
        Ok(reply) => Err(format!(
            "{} answered HTTP {}",
            url.authority(),
            reply.status
        )),
        Err(err) => Err(err.to_string()),
    }
}

fn run_steps(
    ctx: &Context<'_>,
    options: &Options,
    services: &mut Services<'_>,
) -> Result<Outcome, Failure> {
    // 1. The gate: everything that can be known without a request.
    let mcp_url = Url::parse(&ctx.http.url).map_err(|reason: UrlError| {
        fail(
            Reason::NotApplicable,
            format!("the server's URL is not usable: {reason}"),
        )
    })?;
    let canonical = canonical_url(&ctx.http.url);
    if crate::mcp_auth::is_static(ctx.http) {
        return Err(fail(
            Reason::NotApplicable,
            format!(
                "server `{}` has a credential of its own (`bearer_token_env`, `headers_env` or a \
credential header); authorization is for servers that ask for it",
                shown(ctx.name)
            ),
        ));
    }
    if matches!(ctx.http.auth, McpAuthMode::Never) {
        return Err(fail(
            Reason::NotApplicable,
            format!(
                "server `{}` says `auth: false`, so authorization is not started",
                shown(ctx.name)
            ),
        ));
    }
    if !crate::mcp_auth::secure_enough(&ctx.http.url) {
        return Err(fail(
            Reason::NotApplicable,
            "authorization is started for https (or http on this machine) only: a token does not \
cross an unencrypted network",
        ));
    }
    let settings: McpAuthSettings = match &ctx.http.auth {
        McpAuthMode::Settings(settings) => settings.clone(),
        _ => McpAuthSettings::default(),
    };
    // A dry run changes nothing, so agent mode asks nothing of it.
    if ctx.agent && !options.dry_run && !(options.yes && settings.client_id.is_some()) {
        return Err(fail(
            Reason::AgentMode,
            "in agent mode this needs --yes and --non-interactive together, and a server entry \
that names a pre-registered client (`auth.client_id`), so that nothing is registered unattended",
        ));
    }
    if !ctx.interactive && !options.yes && !options.dry_run {
        return Err(fail(
            Reason::ConfirmationRequired,
            "there is no terminal to ask on: pass --yes to authorize without being asked",
        ));
    }
    if !ctx.allow_authorization {
        return Err(fail(
            Reason::Policy,
            "managed policy does not allow authorizing MCP servers (`[mcp] allow_authorization = \
false`)",
        ));
    }
    if let Some(variable) = &settings.client_secret_env
        && !crate::exec_tools::credential_env_permitted(variable, &lookup(ctx.env))
    {
        return Err(fail(
            Reason::NoClient,
            format!(
                "the environment variable {} named by `auth.client_secret_env` is not available \
to a project-declared server: a name starting with MCP_ is, and any other must be listed in {} \
in your own environment",
                shown(variable),
                crate::exec_tools::CREDENTIAL_ENV_ALLOWLIST_VAR
            ),
        ));
    }
    if !crate::mcp_credentials::secure_store_available() {
        return Err(fail(
            Reason::NoSecureStore,
            CredentialError::NoSecureStore.to_string(),
        ));
    }
    // One authorization at a time per user; a dry run reads only.
    let _lock = if options.dry_run {
        None
    } else {
        Some(
            FileLock::acquire(ctx.home.join("mcp-auth.lock"), options.lock_wait).map_err(
                |err| match err {
                    LockError::Busy => fail(
                        Reason::Busy,
                        "another authorization is in progress; wait for it to finish",
                    ),
                    LockError::Io(why) => fail(Reason::Internal, format!("the lock: {why}")),
                },
            )?,
        )
    };

    // 2. Discovery.
    let loopback_http_ok = !mcp_url.is_https() && mcp_url.is_loopback();
    let net = services.net;
    // The server is asked once, with no credential, to see what it says: a
    // challenge may name its metadata and the scope.
    let mut challenge_scope = None;
    let mut challenge_metadata = None;
    let mut probe_error: Option<NetError> = None;
    match net.get(&mcp_url) {
        Ok(reply) => {
            if reply.status == 401 || reply.status == 403 {
                let joined = reply.header_values("www-authenticate").join(", ");
                let parsed = mcp::transport::parse_www_authenticate(&joined);
                challenge_scope = parsed.scope;
                challenge_metadata = parsed.resource_metadata;
            }
        }
        Err(NetError::Cancelled) => return Err(fail(Reason::Cancelled, "cancelled")),
        // A server that answers a bare GET with a stream, a redirect to
        // nowhere or nothing in time has shown no challenge; its well-known
        // locations are still asked, and if they fail too this is said.
        Err(err) => probe_error = Some(err),
    }
    let mut candidates: Vec<Url> = Vec::new();
    if let Some(text) = challenge_metadata
        && let Ok(url) = Url::parse(&text)
        // Only the server's own origin says where its metadata is.
        && url.origin() == mcp_url.origin()
        && check_endpoint("the metadata address", &url, loopback_http_ok).is_ok()
    {
        candidates.push(url);
    }
    for url in [
        mcp_url.well_known("oauth-protected-resource"),
        mcp_url
            .with_path("/")
            .well_known("oauth-protected-resource"),
    ] {
        if !candidates.contains(&url) {
            candidates.push(url);
        }
    }
    // The first location that gives a document this server is described by.
    // One that gives something that is not a document at all (a page, a
    // truncated body) is passed over; one that gives a document about
    // another resource, or with nothing usable, is an answer and ends this.
    let mut resource = None;
    for url in &candidates {
        match fetch(net, url) {
            Ok(body) => match parse_protected_resource(&body, &ctx.http.url, loopback_http_ok) {
                Ok(found) => {
                    resource = Some(found);
                    break;
                }
                Err(ProtocolError::Malformed { .. } | ProtocolError::TooLarge { .. }) => {}
                Err(err) => return Err(protocol(Reason::DiscoveryFailed, &err)),
            },
            Err(_) if (services.cancelled)() => {
                return Err(fail(Reason::Cancelled, "cancelled"));
            }
            Err(_) => {}
        }
    }
    let issuer_url = match resource {
        Some(resource) => resource
            .authorization_servers
            .first()
            .cloned()
            .ok_or_else(|| {
                protocol(
                    Reason::DiscoveryFailed,
                    &ProtocolError::NoAuthorizationServer,
                )
            })?,
        // A server that publishes no resource metadata is its own authorization
        // server (the earlier profile).
        None => mcp_url.with_path("/"),
    };
    let mut metadata_candidates = vec![
        issuer_url.well_known("oauth-authorization-server"),
        issuer_url.well_known("openid-configuration"),
    ];
    if issuer_url.path() != "/" {
        metadata_candidates.push(issuer_url.with_path(&format!(
            "{}/.well-known/openid-configuration",
            issuer_url.path().trim_end_matches('/')
        )));
    }
    let mut found_server = None;
    for url in &metadata_candidates {
        match fetch(net, url) {
            Ok(body) => match parse_authorization_server(&body, &issuer_url, loopback_http_ok) {
                Ok(found) => {
                    found_server = Some(found);
                    break;
                }
                Err(ProtocolError::Malformed { .. } | ProtocolError::TooLarge { .. }) => {}
                Err(err) => {
                    let mut failure = protocol(Reason::DiscoveryFailed, &err);
                    failure.issuer = Some(issuer_url.to_string());
                    return Err(failure);
                }
            },
            Err(_) if (services.cancelled)() => {
                return Err(fail(Reason::Cancelled, "cancelled"));
            }
            Err(_) => {}
        }
    }
    let server: AuthorizationServer = found_server.ok_or_else(|| {
        let mut message = format!(
            "no authorization server metadata was found for {}",
            issuer_url.origin()
        );
        if let Some(err) = &probe_error {
            message.push_str(&format!(
                "; server `{}` itself could not be asked: {err}",
                shown(ctx.name)
            ));
        }
        fail(Reason::DiscoveryFailed, message)
    })?;
    let issuer = server.issuer_id();
    let origin = crate::mcp_auth::origin_of(&canonical);

    // The client the request is made as; nothing is registered yet.
    let earlier = services
        .oauth_store
        .get(&ctx.http.url)
        .filter(|record| record.client_dynamic && record.issuer == issuer);
    let client = if let Some(id) = settings.client_id.clone() {
        ClientPlan::PreRegistered {
            id,
            secret_env: settings.client_secret_env.clone(),
        }
    } else if let Some(record) = earlier {
        ClientPlan::Reused {
            id: record.client_id,
        }
    } else if server.registration_endpoint.is_some() {
        ClientPlan::Register
    } else {
        let mut failure = fail(
            Reason::NoClient,
            "the authorization server offers no client registration and this server's entry names \
no client: set `auth.client_id` to a client registered with it",
        );
        failure.issuer = Some(issuer);
        return Err(failure);
    };

    // A client secret goes to the token endpoint and nowhere else, and only if
    // that is the issuer's own host: a document that moved it to a third
    // host would be sending a variable of the person's to a stranger.
    if matches!(
        &client,
        ClientPlan::PreRegistered {
            secret_env: Some(_),
            ..
        }
    ) && server.token_endpoint.authority() != server.issuer.authority()
    {
        let mut failure = fail(
            Reason::NoClient,
            format!(
                "the token endpoint is on {}, not on the issuer's host {}; a client secret is not \
sent to a third host",
                server.token_endpoint.authority(),
                server.issuer.authority()
            ),
        );
        failure.issuer = Some(issuer);
        return Err(failure);
    }

    // The scopes: the entry's, else the challenge's.
    let scopes: Vec<String> = if !settings.scopes.is_empty() {
        settings.scopes.clone()
    } else {
        let from_challenge = crate::mcp_auth::scopes_of(challenge_scope.as_deref());
        if from_challenge.is_empty() {
            services
                .state_store
                .needs_auth(&ctx.http.url)
                .map(|need| need.scopes)
                .unwrap_or_default()
        } else {
            from_challenge
        }
    };

    // 3. The listener (so the plan names the real redirect) and the plan.
    let listener = if options.dry_run {
        None
    } else {
        Some(Listener::bind(settings.callback_port).map_err(|err| {
            let reason = match err {
                ListenError::PortInUse(_) => Reason::PortInUse,
                ListenError::Failed(_) => Reason::Internal,
            };
            fail(reason, err.to_string())
        })?)
    };
    let redirect = listener.as_ref().map_or_else(
        || {
            settings.callback_port.map_or_else(
                || "http://127.0.0.1:<a free port>/callback".to_owned(),
                |port| format!("http://127.0.0.1:{port}/callback"),
            )
        },
        Listener::redirect_uri,
    );
    let plan = Plan {
        name: ctx.name.to_owned(),
        server: canonical.clone(),
        issuer: issuer.clone(),
        discovered_from: net.contacted(),
        registration_host: server.registration_endpoint.as_ref().map(Url::authority),
        authorization_host: server.authorization_endpoint.authority(),
        token_host: server.token_endpoint.authority(),
        revocation_host: server.revocation_endpoint.as_ref().map(Url::authority),
        scopes: scopes.clone(),
        client: client.clone(),
        redirect: redirect.clone(),
    };
    for line in plan.lines() {
        services.io.say(&line);
    }
    if options.dry_run {
        services
            .io
            .say("dry run: discovery only; nothing was registered, opened or stored");
        return Ok(Outcome::DryRun(plan));
    }
    if !options.yes {
        match services.io.confirm("Continue? [y/N] ") {
            Ok(true) => {}
            Ok(false) => return Err(fail(Reason::Declined, "not authorized: you answered no")),
            Err(NoAnswer) => {
                return Err(fail(
                    Reason::ConfirmationRequired,
                    "there is no answer to the question: pass --yes to authorize without being asked",
                ));
            }
        }
    }
    let listener = listener.expect("a real run bound a listener");

    // 4. The client identity.
    let (client_id, client_dynamic, client_secret): (String, bool, Option<Secret>) = match &client {
        ClientPlan::PreRegistered { id, secret_env } => {
            let secret = match secret_env {
                None => None,
                Some(variable) => match lookup(ctx.env)(variable) {
                    Some(value) if !value.is_empty() => Some(Secret::new(value)),
                    _ => {
                        return Err(fail(
                            Reason::NoClient,
                            format!(
                                "the environment variable {} named by `auth.client_secret_env` is \
not set",
                                shown(variable)
                            ),
                        ));
                    }
                },
            };
            (id.clone(), false, secret)
        }
        ClientPlan::Reused { id } => (id.clone(), true, None),
        ClientPlan::Register => {
            let endpoint = server
                .registration_endpoint
                .as_ref()
                .expect("the plan registers only where there is an endpoint");
            let reply = net
                .post(
                    endpoint,
                    "application/json",
                    None,
                    &registration_body(&redirect, &scopes),
                )
                .map_err(|err| {
                    let mut failure = fail(Reason::NoClient, format!("registration failed: {err}"));
                    failure.issuer = Some(issuer.clone());
                    failure
                })?;
            if !(200..300).contains(&reply.status) {
                let mut failure = fail(
                    Reason::NoClient,
                    format!("registration was refused (HTTP {})", reply.status),
                );
                failure.issuer = Some(issuer.clone());
                return Err(failure);
            }
            let id = parse_registration(&reply.body).map_err(|err| {
                let mut failure = protocol(Reason::NoClient, &err);
                failure.issuer = Some(issuer.clone());
                failure
            })?;
            (id, true, None)
        }
    };

    // 5. The authorization request and the wait for its redirect.
    let internal = |what: &str| {
        fail(
            Reason::Internal,
            format!("{what}: the system's random source did not answer"),
        )
    };
    let state = new_state().map_err(|_| internal("state"))?;
    let pkce = Pkce::generate().map_err(|_| internal("PKCE"))?;
    let scope_text = scopes.join(" ");
    let url = authorization_url(&AuthorizationRequest {
        endpoint: &server.authorization_endpoint,
        client_id: &client_id,
        redirect_uri: &redirect,
        state: &state,
        challenge: pkce.challenge(),
        resource: &canonical,
        scope: (!scope_text.is_empty()).then_some(scope_text.as_str()),
    });
    let pasted = options
        .no_browser
        .then(|| services.io.start_paste())
        .flatten();
    let opened = if options.no_browser || !ctx.interactive || ctx.agent {
        false
    } else {
        match services.opener.open(&url) {
            Ok(()) => {
                net.note(&server.authorization_endpoint.origin(), "opened_in_browser");
                true
            }
            Err(OpenError::Unavailable(why) | OpenError::Failed(why)) => {
                services.io.say(&format!(
                    "could not open a browser ({}); use the address below",
                    shown(&why)
                ));
                false
            }
        }
    };
    if opened {
        services.io.say(&format!(
            "opened {} in your browser; waiting up to {} s for the redirect (Ctrl-C to cancel)",
            server.authorization_endpoint.authority(),
            options.timeout.as_secs()
        ));
    } else {
        net.note(&server.authorization_endpoint.origin(), "address_printed");
        services.io.say("open this address in a browser:");
        services.io.say(&url);
        if pasted.is_some() {
            services.io.say(
                "then, if the browser cannot reach this machine, paste the address it ends on",
            );
        }
        services.io.say(&format!(
            "waiting up to {} s for the redirect (Ctrl-C to cancel)",
            options.timeout.as_secs()
        ));
    }
    let mut notices: Vec<String> = Vec::new();
    let waited = listener.wait(
        &state,
        options.timeout,
        pasted.as_ref(),
        services.cancelled,
        &mut |text| notices.push(text.to_owned()),
    );
    for notice in notices {
        services.io.say(&notice);
    }
    let with_issuer = |mut failure: Failure| {
        failure.issuer = Some(issuer.clone());
        failure
    };
    let (code, redirect_iss) = match waited {
        Waited::Callback(Callback::Code { code, iss }) => (code, iss),
        Waited::Callback(Callback::Denied) => {
            return Err(with_issuer(fail(
                Reason::DeniedByUser,
                "the authorization was refused in the browser",
            )));
        }
        Waited::Callback(Callback::Failed { error }) => {
            return Err(with_issuer(fail(
                Reason::ExchangeFailed,
                format!("the authorization server answered {}", shown(&error)),
            )));
        }
        Waited::TooManyInvalid => {
            return Err(with_issuer(fail(
                Reason::Aborted,
                "five requests that were not the redirect arrived; the authorization was ended",
            )));
        }
        Waited::TimedOut => {
            return Err(with_issuer(fail(
                Reason::Timeout,
                format!(
                    "no redirect arrived within {} s; nothing was stored",
                    options.timeout.as_secs()
                ),
            )));
        }
        Waited::Cancelled => return Err(with_issuer(fail(Reason::Cancelled, "cancelled"))),
    };
    if let Some(iss) = &redirect_iss {
        check_iss(iss, &server.issuer)
            .map_err(|err| with_issuer(protocol(Reason::ExchangeFailed, &err)))?;
    }

    // 6. The exchange.
    let in_body = server.secret_in_body();
    let body = exchange_body(&ExchangeForm {
        code: &code,
        verifier: pkce.verifier(),
        redirect_uri: &redirect,
        client_id: &client_id,
        resource: &canonical,
        secret_in_body: client_secret.as_ref().filter(|_| in_body),
    });
    let basic = client_secret
        .as_ref()
        .filter(|_| !in_body)
        .map(|secret| basic_authorization(&client_id, secret));
    let reply = net
        .post(
            &server.token_endpoint,
            "application/x-www-form-urlencoded",
            basic.as_deref(),
            body.as_bytes(),
        )
        .map_err(|err| {
            with_issuer(fail(
                Reason::ExchangeFailed,
                format!("the token request failed: {err}"),
            ))
        })?;
    let granted = parse_token_reply(reply.status, &reply.body, &server.issuer)
        .map_err(|err| with_issuer(protocol(Reason::ExchangeFailed, &err)))?;

    // 7. The tokens into the credential store, then what is known about them.
    let key = credential_key(&canonical, &issuer, &client_id);
    let previous = services.oauth_store.get(&ctx.http.url);
    put_tokens(&key, &granted.access, granted.refresh.as_ref()).map_err(|err| {
        with_issuer(match err {
            CredentialError::NoSecureStore => fail(
                Reason::NoSecureStore,
                CredentialError::NoSecureStore.to_string(),
            ),
            other => fail(Reason::Internal, other.to_string()),
        })
    })?;
    let granted_scopes = granted.scopes.clone().unwrap_or_else(|| scopes.clone());
    let expires_at_ms = granted
        .expires_in
        .map(|secs| now_ms().saturating_add(i64::try_from(secs).unwrap_or(0).saturating_mul(1000)));
    let record = Authorization::new(
        origin,
        issuer.clone(),
        client_id.clone(),
        client_dynamic,
        granted_scopes.clone(),
        server.token_endpoint.to_string(),
        server.revocation_endpoint.as_ref().map(Url::to_string),
        key.clone(),
        expires_at_ms,
    )
    .with_refresh(granted.refresh.is_some(), in_body);
    if let Err(why) = services.oauth_store.put(&ctx.http.url, record) {
        let _ = crate::mcp_credentials::delete_tokens(&key);
        // A record from an earlier authorization must not go on saying the
        // tokens it names are there.
        if previous.is_some() {
            let _ = services.oauth_store.tokens_gone(&ctx.http.url);
        }
        return Err(with_issuer(fail(
            Reason::Internal,
            format!("the authorization could not be recorded ({why}); the tokens were removed"),
        )));
    }
    // A binding this replaces no longer needs its tokens.
    if let Some(old) = previous
        && old.credential_key != key
    {
        let _ = crate::mcp_credentials::delete_tokens(&old.credential_key);
    }
    let _ = services.state_store.clear(&ctx.http.url);
    Ok(Outcome::Authorized(AuthorizedInfo {
        issuer,
        client_id,
        scopes: granted_scopes,
        expires_in_secs: granted.expires_in,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_credentials::{MemoryKeychain, with_memory_keychain};
    use crate::mcp_oauth::challenge_of;
    use crate::mcp_oauth_stub::{Browse, FakeBrowser, World, WorldOptions};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    struct Io {
        said: Arc<Mutex<Vec<String>>>,
        answers: VecDeque<Result<bool, NoAnswer>>,
        asked: Arc<Mutex<Vec<String>>>,
        paste: Option<Receiver<String>>,
    }

    impl Interaction for Io {
        fn say(&mut self, text: &str) {
            self.said.lock().unwrap().push(text.to_owned());
        }

        fn confirm(&mut self, question: &str) -> Result<bool, NoAnswer> {
            self.asked.lock().unwrap().push(question.to_owned());
            self.answers.pop_front().unwrap_or(Err(NoAnswer))
        }

        fn start_paste(&mut self) -> Option<Receiver<String>> {
            self.paste.take()
        }
    }

    #[derive(Default)]
    struct Recorded(Mutex<Vec<AuditEvent>>);

    impl Audit for Recorded {
        fn record(&self, event: &AuditEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    struct Harness {
        root: PathBuf,
        home: PathBuf,
        oauth: OauthStore,
        state: crate::mcp_auth::AuthStateStore,
        said: Arc<Mutex<Vec<String>>>,
        asked: Arc<Mutex<Vec<String>>>,
        audit: Recorded,
    }

    fn harness(tag: &str) -> Harness {
        let base = std::env::temp_dir().join(format!(
            "rapid-authorize-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let root = base.join("project");
        let home = base.join("home");
        std::fs::create_dir_all(root.join(".rapidlm")).unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Harness {
            oauth: OauthStore::in_home(&home),
            state: crate::mcp_auth::AuthStateStore::in_home(&home),
            root,
            home,
            said: Arc::default(),
            asked: Arc::default(),
            audit: Recorded::default(),
        }
    }

    impl Harness {
        fn said(&self) -> Vec<String> {
            self.said.lock().unwrap().clone()
        }

        fn said_text(&self) -> String {
            self.said().join("\n")
        }

        fn events(&self) -> Vec<AuditEvent> {
            self.audit.0.lock().unwrap().clone()
        }

        /// Every file under the project and the home, as text.
        fn files_text(&self) -> String {
            fn walk(dir: &Path, out: &mut String) {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    return;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else if let Ok(text) = std::fs::read_to_string(&path) {
                        out.push_str(&format!("--- {}\n{text}\n", path.display()));
                    }
                }
            }
            let mut out = String::new();
            walk(&self.root, &mut out);
            walk(&self.home, &mut out);
            out
        }

        fn write_settings(&self) -> (String, String) {
            let a = r#"{"mcpServers":{"remote":{"type":"http","url":"http://x"}}}"#;
            let b = r#"{"mcpServers":{}}"#;
            std::fs::write(self.root.join(".rapidlm/settings.json"), a).unwrap();
            std::fs::write(self.root.join(".claude/settings.json"), b).unwrap();
            (a.to_owned(), b.to_owned())
        }
    }

    struct Flags {
        interactive: bool,
        agent: bool,
        allow: bool,
        env: Vec<(String, String)>,
        answers: Vec<Result<bool, NoAnswer>>,
        paste: Option<Receiver<String>>,
    }

    impl Default for Flags {
        fn default() -> Self {
            Self {
                interactive: true,
                agent: false,
                allow: true,
                env: Vec::new(),
                answers: Vec::new(),
                paste: None,
            }
        }
    }

    fn endpoint(world: &World, auth: McpAuthMode) -> McpHttpEndpoint {
        McpHttpEndpoint {
            url: world.mcp_url(),
            headers: Vec::new(),
            headers_env: Vec::new(),
            bearer_token_env: None,
            auth,
        }
    }

    fn quick() -> Options {
        Options {
            yes: true,
            timeout: Duration::from_secs(20),
            lock_wait: Duration::from_millis(150),
            ..Options::default()
        }
    }

    fn go(
        h: &Harness,
        entry: &McpHttpEndpoint,
        options: &Options,
        flags: Flags,
        browse: Browse,
    ) -> (Result<Outcome, Failure>, FakeBrowser) {
        let url = Url::parse(&entry.url).unwrap();
        let net = crate::mcp_oauth_net::GatedHttp::for_server(
            &url,
            None,
            Some(h.root.clone()),
            llm_router::provider::CancellationToken::new(),
        );
        let browser = FakeBrowser::new(browse);
        let mut io = Io {
            said: h.said.clone(),
            answers: flags.answers.into(),
            asked: h.asked.clone(),
            paste: flags.paste,
        };
        let ctx = Context {
            name: "remote",
            http: entry,
            home: &h.home,
            env: &flags.env,
            interactive: flags.interactive,
            agent: flags.agent,
            allow_authorization: flags.allow,
        };
        let outcome = {
            let mut services = Services {
                net: &net,
                io: &mut io,
                opener: &browser,
                audit: &h.audit,
                cancelled: &|| false,
                oauth_store: &h.oauth,
                state_store: &h.state,
            };
            run(&ctx, options, &mut services)
        };
        (outcome, browser)
    }

    fn keychain_names(keychain: &MemoryKeychain) -> Vec<String> {
        keychain.items.lock().unwrap().keys().cloned().collect()
    }

    fn form_value(forms: &[Vec<(String, String)>], name: &str) -> Option<String> {
        forms
            .last()?
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }

    #[test]
    fn the_whole_flow_with_dynamic_registration_stores_tokens_and_nothing_leaks() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("full");
            let (settings_a, settings_b) = h.write_settings();
            h.state
                .record_needs_auth(&world.mcp_url(), &crate::mcp_auth::NeedsAuth::default())
                .unwrap();
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, browser) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
            let Ok(Outcome::Authorized(info)) = outcome else {
                panic!("{outcome:?}\n{}", h.said_text());
            };
            assert_eq!(info.issuer, world.issuer());
            assert_eq!(info.client_id, "dyn-client-1");
            assert_eq!(info.scopes, ["read"]);
            assert_eq!(info.expires_in_secs, Some(3600));
            assert_eq!(browser.opened.lock().unwrap().len(), 1, "opened once");

            // The registration: a public native client with the listener's redirect.
            let state = world.state.lock().unwrap();
            assert_eq!(state.registered.len(), 1);
            let registered = &state.registered[0].1;
            assert_eq!(registered["token_endpoint_auth_method"], "none");
            let redirect = registered["redirect_uris"][0].as_str().unwrap().to_owned();
            assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
            assert!(redirect.ends_with("/callback"));
            // The request: PKCE S256, state, resource, scope.
            let query = state.authorize_queries.last().unwrap();
            let q = |name: &str| {
                query
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            assert_eq!(q("response_type"), "code");
            assert_eq!(q("code_challenge_method"), "S256");
            assert_eq!(q("redirect_uri"), redirect);
            assert_eq!(q("resource"), canonical_url(&world.mcp_url()));
            assert_eq!(q("scope"), "read");
            assert!(q("state").len() >= 22, "128 bits at least: {}", q("state"));
            let verifier = form_value(&state.token_forms, "code_verifier").unwrap();
            assert_eq!(
                challenge_of(&verifier),
                q("code_challenge"),
                "the challenge matches"
            );
            assert_eq!(
                state.verifier_ok,
                [true],
                "the server verified PKCE and the resource"
            );
            assert_eq!(
                form_value(&state.token_forms, "grant_type").as_deref(),
                Some("authorization_code")
            );
            drop(state);

            // The tokens are in the credential store, the metadata beside them.
            let record = h.oauth.get(&entry.url).expect("recorded");
            assert_eq!(record.issuer, world.issuer());
            assert_eq!(record.client_id, "dyn-client-1");
            assert!(record.client_dynamic && record.tokens);
            assert_eq!(record.token_endpoint, format!("{}/token", world.issuer()));
            assert_eq!(
                keychain.get_text(&format!("mcp-oauth-access:{}", record.credential_key)),
                Some("world-access-canary-1".to_owned())
            );
            assert_eq!(
                keychain.get_text(&format!("mcp-oauth-refresh:{}", record.credential_key)),
                Some("world-refresh-canary-1".to_owned())
            );
            // The request for authorization is no longer outstanding.
            assert_eq!(h.state.needs_auth(&entry.url), None);

            // Settings files are byte-identical; nothing secret is in anything written.
            assert_eq!(
                std::fs::read_to_string(h.root.join(".rapidlm/settings.json")).unwrap(),
                settings_a
            );
            assert_eq!(
                std::fs::read_to_string(h.root.join(".claude/settings.json")).unwrap(),
                settings_b
            );
            let everything = format!("{}\n{}", h.said_text(), h.files_text());
            for canary in [
                "world-access-canary-1",
                "world-refresh-canary-1",
                "world-code-canary",
                "must-never-be-stored-canary",
                verifier.as_str(),
            ] {
                assert!(
                    !everything.contains(canary),
                    "{canary} leaked:\n{everything}"
                );
            }
            // The audit says who and what, never a token.
            let events = h.events();
            assert_eq!(events.len(), 1);
            let line = FileAudit::line(&events[0]);
            assert!(
                line.contains("mcp.authorized") && line.contains("dyn-client-1"),
                "{line}"
            );
            assert!(!line.contains("canary"));
            assert_eq!(keychain_names(keychain).len(), 2);
        });
    }

    #[test]
    fn a_pre_registered_client_works_without_a_registration_and_its_secret_never_leaks() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions {
                client_secret: Some("sekret-canary-7".to_owned()),
                ..WorldOptions::default()
            });
            let h = harness("prereg");
            let entry = endpoint(
                &world,
                McpAuthMode::Settings(McpAuthSettings {
                    client_id: Some("pre-client".to_owned()),
                    client_secret_env: Some("MCP_CLIENT_SECRET".to_owned()),
                    scopes: vec!["write".to_owned()],
                    callback_port: None,
                }),
            );
            let flags = Flags {
                env: vec![("MCP_CLIENT_SECRET".to_owned(), "sekret-canary-7".to_owned())],
                ..Flags::default()
            };
            let (outcome, _) = go(&h, &entry, &quick(), flags, Browse::Follow);
            let Ok(Outcome::Authorized(info)) = outcome else {
                panic!("{outcome:?}\n{}", h.said_text());
            };
            assert_eq!(info.client_id, "pre-client");
            assert_eq!(
                info.scopes,
                ["write"],
                "the entry's scopes win over the challenge's"
            );
            let state = world.state.lock().unwrap();
            assert!(state.registered.is_empty(), "no registration request");
            assert!(world.mcp.requests_to("/register").is_empty());
            // The secret went in a Basic header, once, and nowhere else.
            let header = state.token_authorizations[0].clone().expect("basic");
            assert!(header.starts_with("Basic "));
            assert!(
                state.token_forms[0]
                    .iter()
                    .all(|(k, _)| k != "client_secret")
            );
            drop(state);
            let plan = h.said_text();
            assert!(plan.contains("pre-registered `pre-client`"), "{plan}");
            assert!(
                plan.contains("secret from $MCP_CLIENT_SECRET sent to"),
                "{plan}"
            );
            let everything = format!("{plan}\n{}", h.files_text());
            assert!(!everything.contains("sekret-canary-7"), "{everything}");
            assert!(!h.oauth.get(&entry.url).unwrap().client_dynamic);
            assert_eq!(keychain_names(keychain).len(), 2);
        });
    }

    #[test]
    fn a_client_secret_the_environment_does_not_hold_stops_before_anything_is_asked() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("nosecret");
            let with_variable = |name: &str| {
                endpoint(
                    &world,
                    McpAuthMode::Settings(McpAuthSettings {
                        client_id: Some("pre-client".to_owned()),
                        client_secret_env: Some(name.to_owned()),
                        ..McpAuthSettings::default()
                    }),
                )
            };
            // A name a project may not read.
            let (outcome, _) = go(
                &h,
                &with_variable("AWS_SECRET_ACCESS_KEY"),
                &quick(),
                Flags::default(),
                Browse::Follow,
            );
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::NoClient);
            assert!(
                failure
                    .message
                    .contains("not available to a project-declared server"),
                "{failure}"
            );
            assert!(world.requests().is_empty(), "before any network contact");
            // A name it may read that is not set.
            let (outcome, _) = go(
                &h,
                &with_variable("MCP_UNSET"),
                &quick(),
                Flags::default(),
                Browse::Follow,
            );
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::NoClient);
            assert!(failure.message.contains("MCP_UNSET"), "{failure}");
            assert!(keychain_names(keychain).is_empty());
            assert!(world.state.lock().unwrap().token_forms.is_empty());
        });
    }

    #[test]
    fn a_dry_run_discovers_and_shows_the_plan_and_changes_nothing() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("dry");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let options = Options {
                dry_run: true,
                ..quick()
            };
            // No terminal and no --yes is fine for a dry run: it only reads.
            let flags = Flags {
                interactive: false,
                ..Flags::default()
            };
            let options = Options {
                yes: false,
                ..options
            };
            let (outcome, browser) = go(&h, &entry, &options, flags, Browse::Follow);
            let Ok(Outcome::DryRun(plan)) = outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(plan.client, ClientPlan::Register);
            let state = world.state.lock().unwrap();
            assert!(state.registered.is_empty(), "no client registered");
            assert!(
                state.codes.is_empty() && state.authorize_queries.is_empty(),
                "no code issued"
            );
            assert!(state.token_forms.is_empty());
            drop(state);
            assert!(browser.opened.lock().unwrap().is_empty());
            assert!(
                keychain_names(keychain).is_empty(),
                "the credential store is unchanged"
            );
            assert!(h.oauth.get(&entry.url).is_none());
            assert!(h.events().is_empty(), "a dry run leaves no audit record");
            assert!(h.said_text().contains("dry run: discovery only"));
            assert!(!h.home.join("mcp-oauth.json").exists());
            assert!(
                !h.home.join("mcp-auth.lock").exists(),
                "a dry run takes no lock"
            );
            assert!(h.asked.lock().unwrap().is_empty(), "and asks nothing");
        });
    }

    #[test]
    fn without_a_terminal_and_without_yes_the_command_refuses_before_any_request() {
        with_memory_keychain(|_| {
            let world = World::start(WorldOptions::default());
            let h = harness("noyes");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let flags = Flags {
                interactive: false,
                ..Flags::default()
            };
            let options = Options {
                yes: false,
                ..quick()
            };
            let (outcome, _) = go(&h, &entry, &options, flags, Browse::Follow);
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::ConfirmationRequired);
            assert!(failure.message.contains("--yes"), "{failure}");
            assert!(world.requests().is_empty());
        });
    }

    #[test]
    fn the_plan_names_every_host_the_flow_touches_and_each_contact_has_a_receipt() {
        with_memory_keychain(|_| {
            // The server and its authorization server on two origins.
            let world = World::split(WorldOptions::default());
            let h = harness("plan");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
            assert!(matches!(outcome, Ok(Outcome::Authorized(_))), "{outcome:?}");
            let plan_text = h.said().join("\n");
            let contacts_line = h
                .said()
                .into_iter()
                .find(|line| line.trim_start().starts_with("contacts"))
                .expect("a contacts line");
            let seen = world.authorities_seen();
            assert_eq!(seen.len(), 2, "two origins were involved: {seen:?}");
            for authority in &seen {
                assert!(
                    contacts_line.contains(authority.as_str()),
                    "{authority} not in {contacts_line}"
                );
            }
            // And every one has a receipt.
            let receipts =
                std::fs::read_to_string(h.root.join(".rapidlm/egress-receipts.jsonl")).unwrap();
            for authority in &seen {
                assert!(
                    receipts.lines().any(|line| line.contains("\"mcp_auth\"")
                        && line.contains(authority.as_str())
                        && line.contains("\"allowed\":true")),
                    "{authority}:\n{receipts}"
                );
            }
            // The browser hand-off is receipted too.
            assert!(receipts.contains("opened_in_browser"), "{receipts}");
            assert!(plan_text.contains("registration"), "{plan_text}");
        });
    }

    #[test]
    fn a_wrong_state_never_ends_the_wait_and_nothing_is_stored_when_time_runs_out() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("tamper");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let options = Options {
                timeout: Duration::from_millis(600),
                ..quick()
            };
            let (outcome, _) = go(
                &h,
                &entry,
                &options,
                Flags::default(),
                Browse::FollowChanged(|location| location.replacen("state=", "state=X", 1)),
            );
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::Timeout);
            assert!(
                world.state.lock().unwrap().token_forms.is_empty(),
                "no code was exchanged"
            );
            assert!(keychain_names(keychain).is_empty());
            assert!(h.oauth.get(&entry.url).is_none());
            let events = h.events();
            assert!(
                matches!(
                    &events[..],
                    [AuditEvent::Failed {
                        reason: "timeout",
                        ..
                    }]
                ),
                "{events:?}"
            );
        });
    }

    #[test]
    fn a_refusal_in_the_browser_is_denied_by_user_and_stores_nothing() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions {
                deny: true,
                ..WorldOptions::default()
            });
            let h = harness("deny");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::DeniedByUser);
            assert_eq!(failure.reason.code(), "denied_by_user");
            assert!(keychain_names(keychain).is_empty());
            assert!(world.state.lock().unwrap().token_forms.is_empty());
            let events = h.events();
            assert!(
                matches!(
                    &events[..],
                    [AuditEvent::Failed {
                        reason: "denied_by_user",
                        issuer: Some(_),
                        ..
                    }]
                ),
                "{events:?}"
            );
        });
    }

    #[test]
    fn answering_no_to_the_plan_registers_and_opens_nothing() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("decline");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let options = Options {
                yes: false,
                ..quick()
            };
            let flags = Flags {
                answers: vec![Ok(false)],
                ..Flags::default()
            };
            let (outcome, browser) = go(&h, &entry, &options, flags, Browse::Follow);
            assert_eq!(outcome.unwrap_err().reason, Reason::Declined);
            assert!(world.state.lock().unwrap().registered.is_empty());
            assert!(browser.opened.lock().unwrap().is_empty());
            assert!(keychain_names(keychain).is_empty());
            assert_eq!(h.asked.lock().unwrap().len(), 1, "the question was asked");
            assert!(
                h.events().is_empty(),
                "declining is not an event of the flow"
            );
            // And a question nobody can answer is not a yes.
            let flags = Flags::default();
            let (outcome, _) = go(&h, &entry, &options, flags, Browse::Follow);
            assert_eq!(outcome.unwrap_err().reason, Reason::ConfirmationRequired);
        });
    }

    #[test]
    fn iss_token_type_size_and_https_are_each_refused_with_a_typed_reason() {
        for (label, options, expect, reason) in [
            (
                "iss on the redirect",
                WorldOptions {
                    iss_in_redirect: Some("https://evil.example".to_owned()),
                    ..WorldOptions::default()
                },
                "issuer",
                Reason::ExchangeFailed,
            ),
            (
                "iss on the reply",
                WorldOptions {
                    iss_in_token_reply: Some("https://evil.example".to_owned()),
                    ..WorldOptions::default()
                },
                "issuer",
                Reason::ExchangeFailed,
            ),
            (
                "token type",
                WorldOptions {
                    token_type: "mac".to_owned(),
                    ..WorldOptions::default()
                },
                "Bearer",
                Reason::ExchangeFailed,
            ),
            (
                "oversized token",
                WorldOptions {
                    access_token: "a".repeat(9 * 1024),
                    ..WorldOptions::default()
                },
                "8 KiB",
                Reason::ExchangeFailed,
            ),
            (
                "lifetime",
                WorldOptions {
                    expires_in: Some(serde_json::json!(40 * 24 * 3600)),
                    ..WorldOptions::default()
                },
                "lifetime",
                Reason::ExchangeFailed,
            ),
            (
                "rejected exchange",
                WorldOptions {
                    token_error: Some("invalid_grant".to_owned()),
                    token_status: 400,
                    ..WorldOptions::default()
                },
                "invalid_grant",
                Reason::ExchangeFailed,
            ),
            (
                "plain http authorization server",
                WorldOptions {
                    authorization_servers: Some(vec!["http://as.example".to_owned()]),
                    ..WorldOptions::default()
                },
                "not https",
                Reason::DiscoveryFailed,
            ),
            (
                "another issuer",
                WorldOptions {
                    issuer_in_metadata: Some("https://evil.example".to_owned()),
                    ..WorldOptions::default()
                },
                "issuer",
                Reason::DiscoveryFailed,
            ),
            (
                "no PKCE",
                WorldOptions {
                    pkce_methods: Vec::new(),
                    ..WorldOptions::default()
                },
                "PKCE",
                Reason::DiscoveryFailed,
            ),
            (
                "another resource",
                WorldOptions {
                    prm_resource: Some("https://other.example/mcp".to_owned()),
                    ..WorldOptions::default()
                },
                "describes",
                Reason::DiscoveryFailed,
            ),
        ] {
            with_memory_keychain(|keychain| {
                let world = World::start(options);
                let h = harness("refuse");
                let entry = endpoint(&world, McpAuthMode::Engage);
                let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
                let failure = outcome.expect_err(label);
                assert_eq!(failure.reason, reason, "{label}: {failure}");
                assert!(failure.message.contains(expect), "{label}: {failure}");
                assert!(
                    keychain_names(keychain).is_empty(),
                    "{label}: nothing stored"
                );
                assert!(h.oauth.get(&entry.url).is_none(), "{label}");
                let everything = format!("{failure}\n{}\n{}", h.said_text(), h.files_text());
                assert!(
                    !everything.contains("secret-canary"),
                    "{label}: {everything}"
                );
                assert!(!everything.contains("world-access-canary"), "{label}");
            });
        }
    }

    #[test]
    fn a_pasted_callback_completes_the_flow_without_a_browser() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("paste");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (send, receive) = std::sync::mpsc::channel::<String>();
            // The person's other machine: visit the printed address, and paste
            // what the browser ended on.
            let said = h.said.clone();
            let person = std::thread::spawn(move || {
                for _ in 0..400 {
                    let address = said
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|line| line.contains("/authorize?"))
                        .cloned();
                    if let Some(address) = address {
                        let (_, location) = crate::mcp_oauth_stub::http_get(&address).unwrap();
                        send.send(location.unwrap()).unwrap();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            });
            let options = Options {
                no_browser: true,
                ..quick()
            };
            let flags = Flags {
                paste: Some(receive),
                ..Flags::default()
            };
            let (outcome, browser) = go(&h, &entry, &options, flags, Browse::Follow);
            person.join().unwrap();
            assert!(
                matches!(outcome, Ok(Outcome::Authorized(_))),
                "{outcome:?}\n{}",
                h.said_text()
            );
            assert!(
                browser.opened.lock().unwrap().is_empty(),
                "no browser was opened"
            );
            assert!(
                h.said_text().contains("paste the address"),
                "{}",
                h.said_text()
            );
            assert_eq!(keychain_names(keychain).len(), 2);
            // The printed address carries the single-use state and the PKCE
            // challenge, and never the verifier.
            let state = world.state.lock().unwrap();
            let verifier = form_value(&state.token_forms, "code_verifier").unwrap();
            assert!(!h.said_text().contains(&verifier));
            assert!(h.said_text().contains("code_challenge="));
        });
    }

    #[test]
    fn agent_mode_needs_yes_and_a_pre_registered_client_and_never_opens_a_browser() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("agent");
            let dynamic = endpoint(&world, McpAuthMode::Engage);
            let named = endpoint(
                &world,
                McpAuthMode::Settings(McpAuthSettings {
                    client_id: Some("pre-client".to_owned()),
                    ..McpAuthSettings::default()
                }),
            );
            let agent = || Flags {
                agent: true,
                interactive: false,
                ..Flags::default()
            };
            // Without --yes.
            let (outcome, _) = go(
                &h,
                &named,
                &Options {
                    yes: false,
                    ..quick()
                },
                agent(),
                Browse::Follow,
            );
            assert_eq!(outcome.unwrap_err().reason, Reason::AgentMode);
            // With --yes but nothing pre-registered: no registration unattended.
            let (outcome, _) = go(&h, &dynamic, &quick(), agent(), Browse::Follow);
            assert_eq!(outcome.unwrap_err().reason, Reason::AgentMode);
            assert!(
                world.requests().is_empty(),
                "both refused before any request"
            );
            assert!(world.state.lock().unwrap().registered.is_empty());
            // Both: the redirect can only be completed by hand, and no browser is opened.
            let (send, receive) = std::sync::mpsc::channel::<String>();
            drop(send);
            let (outcome, browser) = go(
                &h,
                &named,
                &Options {
                    timeout: Duration::from_millis(300),
                    ..quick()
                },
                Flags {
                    paste: Some(receive),
                    ..agent()
                },
                Browse::Follow,
            );
            assert_eq!(outcome.unwrap_err().reason, Reason::Timeout);
            assert!(
                browser.opened.lock().unwrap().is_empty(),
                "never opened when non-interactive"
            );
            assert!(
                h.said_text().contains("/authorize?"),
                "the address is printed instead"
            );
            assert!(keychain_names(keychain).is_empty());
            // Agent mode is not a browser mode even if something says a terminal
            // is there.
            let (_send, receive) = std::sync::mpsc::channel::<String>();
            let (outcome, browser) = go(
                &h,
                &named,
                &Options {
                    timeout: Duration::from_millis(300),
                    ..quick()
                },
                Flags {
                    interactive: true,
                    paste: Some(receive),
                    ..agent()
                },
                Browse::Follow,
            );
            assert_eq!(outcome.unwrap_err().reason, Reason::Timeout);
            assert!(
                browser.opened.lock().unwrap().is_empty(),
                "agent mode opens no browser"
            );
        });
    }

    #[test]
    fn no_secure_store_stops_before_any_request_and_writes_nothing() {
        let world = World::start(WorldOptions::default());
        let h = harness("nostore");
        let entry = endpoint(&world, McpAuthMode::Engage);
        let unavailable = Arc::new(MemoryKeychain::unavailable());
        let (outcome, _) = crate::provider_keychain::with_backend(unavailable, || {
            go(&h, &entry, &quick(), Flags::default(), Browse::Follow)
        });
        let failure = outcome.unwrap_err();
        assert_eq!(failure.reason, Reason::NoSecureStore);
        assert!(
            failure.message.contains("never written to a file"),
            "{failure}"
        );
        assert!(world.requests().is_empty());
        assert!(
            h.files_text().is_empty(),
            "no file of any kind: {}",
            h.files_text()
        );
        let events = h.events();
        assert!(matches!(
            &events[..],
            [AuditEvent::Failed {
                reason: "no_secure_store",
                ..
            }]
        ));
    }

    #[test]
    fn a_server_this_command_does_not_apply_to_is_refused_before_any_request() {
        with_memory_keychain(|_| {
            let world = World::start(WorldOptions::default());
            let h = harness("n-a");
            let mut with_token = endpoint(&world, McpAuthMode::Engage);
            with_token.bearer_token_env = Some("MCP_TOKEN".to_owned());
            let never = endpoint(&world, McpAuthMode::Never);
            let mut plain = endpoint(&world, McpAuthMode::Engage);
            plain.url = "http://mcp.corp/mcp".to_owned();
            for (label, endpoint) in [
                ("static", &with_token),
                ("auth false", &never),
                ("plain http", &plain),
            ] {
                let (outcome, _) = go(&h, endpoint, &quick(), Flags::default(), Browse::Follow);
                let failure = outcome.expect_err(label);
                assert_eq!(failure.reason, Reason::NotApplicable, "{label}: {failure}");
            }
            assert!(world.requests().is_empty());
            assert!(
                h.events().is_empty(),
                "a refusal to start is not an event of the flow"
            );
        });
    }

    #[test]
    fn policy_that_forbids_authorization_stops_before_any_request() {
        with_memory_keychain(|_| {
            let world = World::start(WorldOptions::default());
            let h = harness("policy");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let flags = Flags {
                allow: false,
                ..Flags::default()
            };
            let (outcome, _) = go(&h, &entry, &quick(), flags, Browse::Follow);
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::Policy);
            assert!(failure.message.contains("allow_authorization"), "{failure}");
            assert!(world.requests().is_empty(), "the server saw nothing");
            assert!(matches!(
                &h.events()[..],
                [AuditEvent::Failed {
                    reason: "policy",
                    ..
                }]
            ));
        });
    }

    #[test]
    fn a_second_authorization_waits_for_the_first_and_then_says_so() {
        with_memory_keychain(|_| {
            let world = World::start(WorldOptions::default());
            let h = harness("busy");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let _held =
                FileLock::acquire(h.home.join("mcp-auth.lock"), Duration::from_millis(50)).unwrap();
            let started = std::time::Instant::now();
            let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::Busy);
            assert!(
                failure
                    .message
                    .contains("another authorization is in progress"),
                "{failure}"
            );
            assert!(started.elapsed() >= Duration::from_millis(150));
            assert!(world.requests().is_empty());
        });
    }

    #[test]
    fn a_fixed_callback_port_that_is_taken_is_a_typed_error_naming_it() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("port");
            let holder = crate::mcp_oauth_listen::Listener::bind(None).unwrap();
            let port = holder.port();
            let entry = endpoint(
                &world,
                McpAuthMode::Settings(McpAuthSettings {
                    callback_port: Some(port),
                    ..McpAuthSettings::default()
                }),
            );
            let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::PortInUse);
            assert!(failure.message.contains(&port.to_string()), "{failure}");
            assert!(
                world.state.lock().unwrap().registered.is_empty(),
                "nothing was registered"
            );
            assert!(keychain_names(keychain).is_empty());
        });
    }

    #[test]
    fn a_server_with_no_registration_and_no_named_client_is_no_client_with_a_hint() {
        with_memory_keychain(|_| {
            let world = World::start(WorldOptions {
                registration: false,
                ..WorldOptions::default()
            });
            let h = harness("noclient");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::NoClient);
            assert!(failure.message.contains("auth.client_id"), "{failure}");
        });
    }

    #[test]
    fn a_registration_that_is_refused_or_not_public_is_no_client() {
        for (label, options) in [
            (
                "refused",
                WorldOptions {
                    registration_status: 403,
                    ..WorldOptions::default()
                },
            ),
            (
                "confidential",
                WorldOptions {
                    registration_method: "client_secret_basic".to_owned(),
                    ..WorldOptions::default()
                },
            ),
        ] {
            with_memory_keychain(|keychain| {
                let world = World::start(options);
                let h = harness("badreg");
                let entry = endpoint(&world, McpAuthMode::Engage);
                let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
                let failure = outcome.expect_err(label);
                assert_eq!(failure.reason, Reason::NoClient, "{label}: {failure}");
                assert!(keychain_names(keychain).is_empty(), "{label}");
                assert!(world.state.lock().unwrap().token_forms.is_empty());
            });
        }
    }

    #[test]
    fn a_client_registered_earlier_is_reused_and_a_replaced_binding_loses_its_tokens() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("reuse");
            let dynamic = endpoint(&world, McpAuthMode::Engage);
            let (first, _) = go(&h, &dynamic, &quick(), Flags::default(), Browse::Follow);
            assert!(matches!(first, Ok(Outcome::Authorized(_))));
            assert_eq!(world.state.lock().unwrap().registered.len(), 1);
            let (second, _) = go(&h, &dynamic, &quick(), Flags::default(), Browse::Follow);
            let Ok(Outcome::Authorized(info)) = second else {
                panic!("{second:?}");
            };
            assert_eq!(info.client_id, "dyn-client-1", "the same client");
            assert_eq!(
                world.state.lock().unwrap().registered.len(),
                1,
                "registered once"
            );
            assert!(h.said_text().contains("registered earlier `dyn-client-1`"));
            assert_eq!(keychain_names(keychain).len(), 2, "one binding, two tokens");
            // The entry now names another client: a different binding, and the
            // old one's tokens are removed.
            let named = endpoint(
                &world,
                McpAuthMode::Settings(McpAuthSettings {
                    client_id: Some("pre-client".to_owned()),
                    ..McpAuthSettings::default()
                }),
            );
            let before = keychain_names(keychain);
            let (third, _) = go(&h, &named, &quick(), Flags::default(), Browse::Follow);
            assert!(matches!(third, Ok(Outcome::Authorized(_))), "{third:?}");
            let after = keychain_names(keychain);
            assert_eq!(after.len(), 2, "{after:?}");
            assert!(
                after.iter().all(|name| !before.contains(name)),
                "the old binding's tokens are gone"
            );
        });
    }

    #[test]
    fn a_client_registered_with_another_issuer_is_not_reused() {
        with_memory_keychain(|_| {
            let world = World::start(WorldOptions::default());
            let h = harness("otherissuer");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let mut record = Authorization::new(
                "http://127.0.0.1".to_owned(),
                "https://elsewhere.example".to_owned(),
                "elsewhere-client".to_owned(),
                true,
                Vec::new(),
                "https://elsewhere.example/token".to_owned(),
                None,
                "key".to_owned(),
                None,
            );
            record.tokens = false;
            h.oauth.put(&entry.url, record).unwrap();
            let options = Options {
                dry_run: true,
                ..quick()
            };
            let (outcome, _) = go(&h, &entry, &options, Flags::default(), Browse::Follow);
            let Ok(Outcome::DryRun(plan)) = outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(
                plan.client,
                ClientPlan::Register,
                "a client of another issuer is not ours"
            );
            // A client registered with this issuer is.
            let mut same = h.oauth.get(&entry.url).unwrap();
            same.issuer = world.issuer();
            h.oauth.put(&entry.url, same).unwrap();
            let (outcome, _) = go(&h, &entry, &options, Flags::default(), Browse::Follow);
            let Ok(Outcome::DryRun(plan)) = outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(
                plan.client,
                ClientPlan::Reused {
                    id: "elsewhere-client".to_owned()
                }
            );
        });
    }

    #[test]
    fn the_servers_challenge_names_its_metadata_only_on_its_own_origin() {
        with_memory_keychain(|_| {
            // On its own origin: followed, and the default location is not tried.
            let world = World::start(WorldOptions {
                prm_alias: Some("/meta/prm.json".to_owned()),
                ..WorldOptions::default()
            });
            let h = harness("alias");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let options = Options {
                dry_run: true,
                ..quick()
            };
            let (outcome, _) = go(&h, &entry, &options, Flags::default(), Browse::Follow);
            assert!(matches!(outcome, Ok(Outcome::DryRun(_))), "{outcome:?}");
            assert_eq!(world.mcp.requests_to("/meta/prm.json").len(), 1);
            assert!(
                world
                    .mcp
                    .requests_to("/.well-known/oauth-protected-resource/mcp")
                    .is_empty()
            );
            // Somewhere else: not asked.
            let other = HttpStubGuard::start();
            let world = World::start(WorldOptions {
                challenge_metadata_url: Some(other.url("/prm")),
                ..WorldOptions::default()
            });
            let h = harness("alias2");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, _) = go(&h, &entry, &options, Flags::default(), Browse::Follow);
            assert!(matches!(outcome, Ok(Outcome::DryRun(_))), "{outcome:?}");
            assert!(
                other.requests().is_empty(),
                "another origin's address is not followed"
            );
        });
    }

    /// A server that must never be asked.
    struct HttpStubGuard(crate::mcp_oauth_stub::HttpStub);

    impl HttpStubGuard {
        fn start() -> Self {
            Self(crate::mcp_oauth_stub::HttpStub::start(|_| {
                crate::mcp_oauth_stub::Response::json(200, "{}")
            }))
        }

        fn url(&self, path: &str) -> String {
            self.0.url(path)
        }

        fn requests(&self) -> Vec<crate::mcp_oauth_stub::Request> {
            self.0.requests()
        }
    }

    #[test]
    fn a_server_that_publishes_no_resource_metadata_is_its_own_authorization_server() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions {
                prm_status: 404,
                ..WorldOptions::default()
            });
            let h = harness("legacy");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
            assert!(
                matches!(outcome, Ok(Outcome::Authorized(_))),
                "{outcome:?}\n{}",
                h.said_text()
            );
            assert_eq!(keychain_names(keychain).len(), 2);
        });
    }

    #[test]
    fn agent_mode_may_look_at_a_plan_without_a_yes_or_a_named_client() {
        with_memory_keychain(|keychain| {
            let world = World::start(WorldOptions::default());
            let h = harness("agentdry");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let flags = Flags {
                agent: true,
                interactive: false,
                ..Flags::default()
            };
            let options = Options {
                dry_run: true,
                yes: false,
                ..quick()
            };
            let (outcome, _) = go(&h, &entry, &options, flags, Browse::Follow);
            assert!(matches!(outcome, Ok(Outcome::DryRun(_))), "{outcome:?}");
            assert!(world.state.lock().unwrap().registered.is_empty());
            assert!(keychain_names(keychain).is_empty());
        });
    }

    #[test]
    fn a_client_secret_is_not_sent_to_a_host_other_than_the_issuers() {
        with_memory_keychain(|keychain| {
            let elsewhere = crate::mcp_oauth_stub::HttpStub::start(|_| {
                crate::mcp_oauth_stub::Response::json(200, "{}")
            });
            let world = World::start(WorldOptions {
                token_endpoint: Some(elsewhere.url("/token")),
                ..WorldOptions::default()
            });
            let h = harness("thirdhost");
            let secret_entry = |name: Option<&str>| {
                endpoint(
                    &world,
                    McpAuthMode::Settings(McpAuthSettings {
                        client_id: Some("pre-client".to_owned()),
                        client_secret_env: name.map(str::to_owned),
                        ..McpAuthSettings::default()
                    }),
                )
            };
            let flags = || Flags {
                env: vec![("MCP_CLIENT_SECRET".to_owned(), "sekret-canary-7".to_owned())],
                ..Flags::default()
            };
            let (outcome, _) = go(
                &h,
                &secret_entry(Some("MCP_CLIENT_SECRET")),
                &quick(),
                flags(),
                Browse::Follow,
            );
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::NoClient);
            assert!(
                failure.message.contains("not on the issuer's host"),
                "{failure}"
            );
            assert!(elsewhere.requests().is_empty(), "nothing went there");
            assert!(world.state.lock().unwrap().token_forms.is_empty());
            assert!(keychain_names(keychain).is_empty());
            // With no secret to send, the same document is no harm to the
            // person: the public client goes on.
            let (outcome, _) = go(
                &h,
                &secret_entry(None),
                &Options {
                    dry_run: true,
                    ..quick()
                },
                flags(),
                Browse::Follow,
            );
            assert!(matches!(outcome, Ok(Outcome::DryRun(_))), "{outcome:?}");
        });
    }

    #[test]
    fn discovery_passes_over_an_unreadable_probe_and_a_page_where_a_document_should_be() {
        with_memory_keychain(|_| {
            let options = Options {
                dry_run: true,
                ..quick()
            };
            // The bare GET of the server goes nowhere: no challenge was seen,
            // and the well-known locations are asked all the same.
            let world = World::start(WorldOptions {
                mcp_get_redirect: Some("http://127.0.0.1:1/nowhere".to_owned()),
                ..WorldOptions::default()
            });
            let h = harness("probefails");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, _) = go(&h, &entry, &options, Flags::default(), Browse::Follow);
            assert!(matches!(outcome, Ok(Outcome::DryRun(_))), "{outcome:?}");
            // The address the challenge names answers with a page: the next
            // location is tried.
            let world = World::start(WorldOptions {
                prm_alias: Some("/meta/prm.json".to_owned()),
                prm_alias_body: Some("<html>not a document</html>".to_owned()),
                ..WorldOptions::default()
            });
            let h = harness("htmlfirst");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, _) = go(&h, &entry, &options, Flags::default(), Browse::Follow);
            assert!(matches!(outcome, Ok(Outcome::DryRun(_))), "{outcome:?}");
            assert_eq!(world.mcp.requests_to("/meta/prm.json").len(), 1);
            assert_eq!(
                world
                    .mcp
                    .requests_to("/.well-known/oauth-protected-resource/mcp")
                    .len(),
                1
            );
            // A server that cannot be reached at all says so in the end.
            let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = free.local_addr().unwrap().port();
            drop(free);
            let h = harness("down");
            let entry = McpHttpEndpoint {
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: Vec::new(),
                headers_env: Vec::new(),
                bearer_token_env: None,
                auth: McpAuthMode::Engage,
            };
            let (outcome, _) = go(&h, &entry, &options, Flags::default(), Browse::Follow);
            let failure = outcome.unwrap_err();
            assert_eq!(failure.reason, Reason::DiscoveryFailed);
            assert!(
                failure.message.contains("itself could not be asked"),
                "{failure}"
            );
        });
    }

    #[test]
    fn the_scope_comes_from_the_entry_else_the_challenge_else_nothing() {
        with_memory_keychain(|_| {
            let h = harness("scope");
            let scope_of = |options: WorldOptions, auth: McpAuthMode| {
                let world = World::start(options);
                let entry = endpoint(&world, auth);
                let (outcome, _) = go(
                    &h,
                    &entry,
                    &Options {
                        dry_run: true,
                        ..quick()
                    },
                    Flags::default(),
                    Browse::Follow,
                );
                let Ok(Outcome::DryRun(plan)) = outcome else {
                    panic!("{outcome:?}");
                };
                plan.scopes
            };
            assert_eq!(
                scope_of(WorldOptions::default(), McpAuthMode::Engage),
                ["read"]
            );
            assert_eq!(
                scope_of(
                    WorldOptions::default(),
                    McpAuthMode::Settings(McpAuthSettings {
                        scopes: vec!["a".to_owned(), "b".to_owned()],
                        ..McpAuthSettings::default()
                    })
                ),
                ["a", "b"]
            );
            assert!(
                scope_of(
                    WorldOptions {
                        challenge_scope: None,
                        ..WorldOptions::default()
                    },
                    McpAuthMode::Engage
                )
                .is_empty()
            );
        });
    }

    #[test]
    fn a_failure_leaves_a_receipt_and_an_audit_line_without_a_secret() {
        let world = World::start(WorldOptions {
            token_error: Some("invalid_grant".to_owned()),
            token_status: 400,
            ..WorldOptions::default()
        });
        with_memory_keychain(|_| {
            let h = harness("audit");
            let entry = endpoint(&world, McpAuthMode::Engage);
            let (outcome, _) = go(&h, &entry, &quick(), Flags::default(), Browse::Follow);
            assert_eq!(outcome.unwrap_err().reason, Reason::ExchangeFailed);
            let audit = FileAudit::in_home(&h.home);
            for event in h.events() {
                audit.record(&event);
            }
            let text = std::fs::read_to_string(h.home.join(AUDIT_FILE)).unwrap();
            assert!(text.contains("\"event\":\"mcp.auth_failed\""), "{text}");
            assert!(text.contains("\"reason\":\"exchange_failed\""), "{text}");
            assert!(
                !text.contains("canary") && !text.contains("invalid_grant"),
                "{text}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(h.home.join(AUDIT_FILE))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o077, 0, "{mode:o}");
            }
        });
    }

    #[test]
    fn no_tool_a_model_is_offered_starts_the_flow() {
        let dir = std::env::temp_dir().join(format!("rapid-auth-tools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let names = crate::exec_tools::builtin_tool_names(&dir).expect("tools");
        assert!(names.len() > 10, "the real table: {names:?}");
        for name in &names {
            let lower = name.to_ascii_lowercase();
            assert!(
                !lower.contains("auth") && !lower.contains("login") && !lower.contains("oauth"),
                "{name} could start an authorization"
            );
        }
    }

    #[test]
    fn the_flow_is_started_from_the_command_and_nowhere_a_model_hook_or_goal_reaches() {
        // The places in the source that call into the flow, by name. The
        // command (`p9_commands`, `mcp_admin`) and the flow's own modules are
        // the flow; the session loop calls it from one place, the slash
        // command a person types.
        let allowed = [
            "mcp_admin.rs",
            "mcp_authorize.rs",
            "mcp_oauth_listen.rs",
            "mcp_oauth_stub.rs",
            "p9_commands.rs",
        ];
        fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&source, &mut files);
        assert!(
            files.len() > 60,
            "the whole of the crate is scanned: {}",
            files.len()
        );
        assert!(
            files.iter().any(|path| path.ends_with("headless/mod.rs")),
            "and its subdirectories"
        );
        let mut offenders = Vec::new();
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if allowed.contains(&name.as_str()) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            // Tests may build what they need; only the code before the tests
            // module counts (and not just the code before the first test-only
            // item: a file is cut where its tests module begins).
            let production = text
                .split("\n#[cfg(test)]\nmod tests")
                .next()
                .unwrap_or_default();
            let session_loop = name == "interactive.rs";
            for needle in [
                "mcp_authorize::run(",
                "mcp_admin::auth(",
                "run_auth_from_process(",
                "mcp_oauth_listen::",
                "mcp_admin::auth_with(",
            ] {
                let count = production.matches(needle).count();
                // The session loop's one use: the person's `/mcp auth`.
                let permitted = match (session_loop, needle) {
                    (true, "mcp_admin::auth_with(") => 1,
                    (true, "mcp_oauth_listen::") => 3,
                    _ => 0,
                };
                if count > permitted {
                    offenders.push(format!("{name}: {needle} x{count}"));
                }
            }
            if session_loop {
                // And that one use is behind the one dispatch of the typed command.
                let dispatches = production.matches("KernelAction::AuthMcp").count();
                if dispatches != 1 {
                    offenders.push(format!("{name}: KernelAction::AuthMcp x{dispatches}"));
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:?}");
    }
}
