//! The requests of the authorization flow (EXT-02-2), every one through an
//! egress gate (S10): discovery, registration, the token exchange and — in a
//! later slice — refresh and revocation.
//!
//! A request is made only after its URL has been judged: `https`, or `http` on
//! this machine for a server that is itself on this machine. It goes through
//! the same HTTP/1.1 + TLS client provider calls use, so the same address
//! guards apply: a name a document supplies cannot lead to a private address.
//! A redirect is followed at most three times and each hop is judged again,
//! so an `https` URL cannot be downgraded and a public name cannot be walked
//! to the local network. Every request, allowed or refused, leaves a receipt.
//!
//! No request carries a credential except the token endpoint's, and nothing
//! here prints a reply: a reply is data for `mcp_oauth` to read.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use llm_router::provider::{CancellationToken, ProviderError};
use llm_router::providers::dial::{DialGate, ProxyConfig};
use llm_router::providers::openai_compatible::{Http1Transport, StaticWireAuth};
use security::NetworkClient;

use crate::mcp_oauth::{MAX_DOCUMENT_BYTES, MAX_REDIRECTS, REQUEST_TIMEOUT, Url, shown};
use crate::provider_egress::{ProviderEgress, host_is_local_literal};

/// What came back.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    /// Every value of the header `name`, in order.
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// Why a request of the flow did not produce a reply. Every text is safe to
/// show: a host is printed, never a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    /// The URL is not one the flow may contact.
    NotHttps { url: String },
    /// The egress policy refused the connection.
    Refused { origin: String },
    /// The connection or the exchange failed.
    Unreachable { origin: String },
    /// No answer within ten seconds.
    TimedOut { origin: String },
    /// The reply is larger than 64 KiB.
    TooLarge { origin: String },
    /// The person ended the flow.
    Cancelled,
    /// More than three redirects.
    TooManyRedirects,
    /// A redirect to something that is not a URL, or that the flow may not
    /// contact.
    BadRedirect { why: String },
    /// A request that is not a GET answered with a redirect: not followed.
    Redirected { origin: String },
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotHttps { url } => write!(
                f,
                "{} is not https (plain http is accepted only on this machine, for a server that \
is itself on this machine)",
                shown(url)
            ),
            Self::Refused { origin } => {
                write!(f, "the egress policy refused a connection to {origin}")
            }
            Self::Unreachable { origin } => write!(f, "{origin} could not be reached"),
            Self::TimedOut { origin } => write!(f, "{origin} did not answer within 10 seconds"),
            Self::TooLarge { origin } => {
                write!(f, "{origin} answered with more than 64 KiB")
            }
            Self::Cancelled => f.write_str("cancelled"),
            Self::TooManyRedirects => {
                write!(f, "more than {MAX_REDIRECTS} redirects")
            }
            Self::BadRedirect { why } => write!(f, "a redirect was refused: {}", shown(why)),
            Self::Redirected { origin } => {
                write!(
                    f,
                    "{origin} answered a POST with a redirect, which is not followed"
                )
            }
        }
    }
}

impl std::error::Error for NetError {}

/// The requests the flow makes.
pub trait OauthHttp {
    /// A GET with no credential, following up to three redirects.
    fn get(&self, url: &Url) -> Result<Reply, NetError>;

    /// A POST; `authorization` is the whole `Authorization` value of a client
    /// that authenticates with one. Not redirected.
    fn post(
        &self,
        url: &Url,
        content_type: &str,
        authorization: Option<&str>,
        body: &[u8],
    ) -> Result<Reply, NetError>;

    /// The hosts (`host[:port]`) requests have gone to so far, each once, in
    /// the order they were first contacted.
    fn contacted(&self) -> Vec<String> {
        Vec::new()
    }

    /// Leave a receipt for something that is not a request of this client (the
    /// browser being sent to an address, say).
    fn note(&self, _dialled: &str, _reason: &str) {}
}

/// The production client: the provider HTTP client behind a gate per hop.
pub struct GatedHttp {
    /// `http` on this machine is allowed: the server being authorized is
    /// itself `http` on this machine.
    loopback_http_ok: bool,
    /// The server being authorized is on this machine (or its private
    /// network, by an address the operator wrote): the endpoints its documents
    /// name may be too. A hosted server's may not.
    trusted_local: bool,
    proxy: Option<ProxyConfig>,
    /// Where receipts go (`<root>/.rapidlm/egress-receipts.jsonl`).
    receipt_root: Option<PathBuf>,
    cancel: CancellationToken,
    /// `host[:port]` of every request made, in order.
    contacted: Mutex<Vec<String>>,
}

impl GatedHttp {
    /// A client for authorizing the server at `mcp_url`.
    pub fn for_server(
        mcp_url: &Url,
        proxy: Option<ProxyConfig>,
        receipt_root: Option<PathBuf>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            loopback_http_ok: !mcp_url.is_https() && mcp_url.is_loopback(),
            trusted_local: host_is_local_literal(mcp_url.host()),
            proxy,
            receipt_root,
            cancel,
            contacted: Mutex::new(Vec::new()),
        }
    }

    /// Whether `http` on this machine is accepted by this client.
    pub fn loopback_http_ok(&self) -> bool {
        self.loopback_http_ok
    }

    /// The hosts contacted so far, each once, in the order first contacted.
    pub fn contacted(&self) -> Vec<String> {
        let all = self
            .contacted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut once: Vec<String> = Vec::new();
        for host in all.iter() {
            if !once.contains(host) {
                once.push(host.clone());
            }
        }
        once
    }

    fn receipt(&self, allowed: bool, dialled: String, reason: Option<&str>) {
        if let Some(root) = &self.receipt_root {
            crate::web_search::record(
                root,
                &[crate::web_search::Receipt {
                    tool: "mcp_auth",
                    allowed,
                    dialled,
                    reason: reason.map(str::to_owned),
                }],
            );
        }
    }

    fn judge(&self, url: &Url) -> Result<(), NetError> {
        if crate::mcp_oauth::check_endpoint("a flow endpoint", url, self.loopback_http_ok).is_ok() {
            return Ok(());
        }
        self.receipt(false, url.origin(), Some("not_https"));
        Err(NetError::NotHttps {
            url: url.to_string(),
        })
    }

    /// One exchange with one URL, no redirect followed.
    fn once(
        &self,
        url: &Url,
        post: Option<(&str, Option<&str>, &[u8])>,
    ) -> Result<llm_router::providers::openai_compatible::RawHttpResponse, NetError> {
        self.judge(url)?;
        let origin = url.origin();
        let host = url.host().trim_start_matches('[').trim_end_matches(']');
        // A local address is reachable only for a server that is itself local;
        // an IPv6 literal is not supported at all. Judged here, ahead of the
        // gate, because a policy rule that names an address literally allows
        // it, and a document must not be able to write that rule.
        let local = host_is_local_literal(host);
        if host.contains(':') || (local && !self.trusted_local) {
            self.receipt(false, origin.clone(), Some("local_address"));
            return Err(NetError::Refused { origin });
        }
        let via = self
            .proxy
            .as_ref()
            .and_then(|proxy| proxy.for_target(url.is_https(), host, url.port()))
            .map(|target| (target.host().to_owned(), target.port()));
        let gate = Arc::new(
            ProviderEgress::for_remote_via(
                NetworkClient::Tool,
                url.is_https(),
                host,
                url.port(),
                via.as_ref().map(|(host, port)| (host.as_str(), *port)),
                self.trusted_local && local,
            )
            .map_err(|_| NetError::Refused {
                origin: origin.clone(),
            })?,
        );
        let mut transport = Http1Transport::with_limits(
            // The constructor wants a wire credential; the raw calls below
            // write their own `Authorization` (or none), never this one.
            StaticWireAuth::bearer("unused".to_owned()).map_err(|_| NetError::Unreachable {
                origin: origin.clone(),
            })?,
            REQUEST_TIMEOUT,
            MAX_DOCUMENT_BYTES,
        )
        .with_dial_gate(Arc::clone(&gate) as Arc<dyn DialGate>);
        if let Some(proxy) = &self.proxy {
            transport = transport.with_proxy(proxy.clone());
        }
        self.contacted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(url.authority());
        let started = Instant::now();
        let target = url.to_string();
        let result = match post {
            None => transport.get_raw(
                &target,
                &[("Accept".to_owned(), "application/json".to_owned())],
                &self.cancel,
            ),
            Some((content_type, authorization, body)) => transport.post_raw_authorized(
                &target,
                &[
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Content-Type".to_owned(), content_type.to_owned()),
                ],
                body,
                authorization,
                &self.cancel,
            ),
        };
        let receipts = gate.drain_receipts();
        if let Some(root) = &self.receipt_root {
            let receipts: Vec<crate::web_search::Receipt> = receipts
                .iter()
                .map(|receipt| crate::web_search::Receipt {
                    tool: "mcp_auth",
                    allowed: receipt.allowed,
                    dialled: receipt.dialled.clone(),
                    reason: receipt.reason.clone(),
                })
                .collect();
            crate::web_search::record(root, &receipts);
        }
        result.map_err(|err| {
            if receipts.iter().any(|receipt| !receipt.allowed) {
                return NetError::Refused {
                    origin: origin.clone(),
                };
            }
            match err {
                ProviderError::Cancelled => NetError::Cancelled,
                ProviderError::BoundExceeded => NetError::TooLarge {
                    origin: origin.clone(),
                },
                _ if started.elapsed() >= REQUEST_TIMEOUT => NetError::TimedOut {
                    origin: origin.clone(),
                },
                _ => NetError::Unreachable {
                    origin: origin.clone(),
                },
            }
        })
    }
}

impl OauthHttp for GatedHttp {
    fn get(&self, url: &Url) -> Result<Reply, NetError> {
        let mut current = url.clone();
        for _hop in 0..=MAX_REDIRECTS {
            let response = self.once(&current, None)?;
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                let Some(location) = response.header("location") else {
                    return Err(NetError::BadRedirect {
                        why: "no Location".to_owned(),
                    });
                };
                if _hop == MAX_REDIRECTS {
                    return Err(NetError::TooManyRedirects);
                }
                current = current
                    .join(location)
                    .map_err(|reason| NetError::BadRedirect {
                        why: reason.to_string(),
                    })?;
                continue;
            }
            return Ok(Reply {
                status: response.status,
                headers: response.headers,
                body: response.body,
            });
        }
        Err(NetError::TooManyRedirects)
    }

    fn post(
        &self,
        url: &Url,
        content_type: &str,
        authorization: Option<&str>,
        body: &[u8],
    ) -> Result<Reply, NetError> {
        let response = self.once(url, Some((content_type, authorization, body)))?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            return Err(NetError::Redirected {
                origin: url.origin(),
            });
        }
        Ok(Reply {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }

    fn contacted(&self) -> Vec<String> {
        GatedHttp::contacted(self)
    }

    fn note(&self, dialled: &str, reason: &str) {
        self.receipt(true, dialled.to_owned(), Some(reason));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_oauth_stub::{HttpStub, Response};

    fn client(stub_url: &str) -> (GatedHttp, tempdir::Dir) {
        let dir = tempdir::Dir::new("net");
        let url = Url::parse(stub_url).unwrap();
        (
            GatedHttp::for_server(
                &url,
                None,
                Some(dir.path().to_path_buf()),
                CancellationToken::new(),
            ),
            dir,
        )
    }

    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "rapid-oauth-net-{tag}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or_default()
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn receipts(dir: &tempdir::Dir) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.path().join(".rapidlm/egress-receipts.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_get_returns_status_and_body_with_no_credential_and_leaves_a_receipt() {
        let stub = HttpStub::start(|request| {
            assert!(request.header("authorization").is_none(), "no credential");
            assert_eq!(request.header("accept"), Some("application/json"));
            Response::json(200, r#"{"ok":true}"#)
        });
        let (http, dir) = client(&stub.url("/"));
        let reply = http.get(&Url::parse(&stub.url("/doc")).unwrap()).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, br#"{"ok":true}"#);
        let logged = receipts(&dir);
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert_eq!(logged[0]["tool"], "mcp_auth");
        assert_eq!(logged[0]["allowed"], true);
        assert_eq!(
            http.contacted(),
            [Url::parse(&stub.url("/")).unwrap().authority()]
        );
    }

    #[test]
    fn a_post_sends_the_content_type_and_the_authorization_it_was_given() {
        let stub = HttpStub::start(|request| {
            assert_eq!(request.method, "POST");
            assert_eq!(
                request.header("content-type"),
                Some("application/x-www-form-urlencoded")
            );
            assert_eq!(request.header("authorization"), Some("Basic abc"));
            assert_eq!(request.body, b"a=1");
            Response::json(200, "{}")
        });
        let (http, _dir) = client(&stub.url("/"));
        let url = Url::parse(&stub.url("/token")).unwrap();
        let reply = http
            .post(
                &url,
                "application/x-www-form-urlencoded",
                Some("Basic abc"),
                b"a=1",
            )
            .unwrap();
        assert_eq!(reply.status, 200);
        let none = HttpStub::start(|request| {
            assert!(request.header("authorization").is_none());
            Response::json(200, "{}")
        });
        let (http, _dir) = client(&none.url("/"));
        http.post(
            &Url::parse(&none.url("/token")).unwrap(),
            "application/json",
            None,
            b"{}",
        )
        .unwrap();
    }

    #[test]
    fn plain_http_off_this_machine_is_refused_before_any_dial_and_receipted() {
        let (http, dir) = client("https://mcp.example/mcp");
        let err = http
            .get(&Url::parse("http://as.example/.well-known/x").unwrap())
            .unwrap_err();
        assert!(matches!(err, NetError::NotHttps { .. }), "{err}");
        let logged = receipts(&dir);
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0]["allowed"], false);
        assert_eq!(logged[0]["reason"], "not_https");
        assert!(http.contacted().is_empty(), "nothing was dialled");
        // A hosted server's documents cannot lead to plain http on this
        // machine either.
        assert!(matches!(
            http.get(&Url::parse("http://127.0.0.1:1/").unwrap()),
            Err(NetError::NotHttps { .. })
        ));
    }

    #[test]
    fn a_hosted_servers_documents_cannot_lead_to_a_local_address_even_over_https() {
        let (http, dir) = client("https://mcp.example/mcp");
        let err = http
            .get(&Url::parse("https://127.0.0.1:9/.well-known/x").unwrap())
            .unwrap_err();
        let logged = receipts(&dir);
        assert!(matches!(err, NetError::Refused { .. }), "{err} {logged:?}");
        assert!(logged.iter().any(|r| r["allowed"] == false), "{logged:?}");
    }

    #[test]
    fn a_redirect_is_followed_and_every_hop_is_judged_again() {
        let target = HttpStub::start(|_| Response::json(200, r#"{"at":"target"}"#));
        let target_url = target.url("/final");
        let hop = HttpStub::start(move |_| Response::redirect(302, &target_url));
        let (http, _dir) = client(&hop.url("/"));
        let reply = http.get(&Url::parse(&hop.url("/start")).unwrap()).unwrap();
        assert_eq!(reply.body, br#"{"at":"target"}"#);
        assert_eq!(http.contacted().len(), 2);

        // A downgrade to plain http off this machine is refused: the server
        // being authorized is hosted, so only https passes.
        let refuse = HttpStub::start(|_| Response::redirect(302, "http://elsewhere.example/x"));
        let (http, _dir) = client(&refuse.url("/"));
        // The stub is on this machine, so the first hop is allowed.
        let err = http
            .get(&Url::parse(&refuse.url("/start")).unwrap())
            .unwrap_err();
        assert!(matches!(err, NetError::NotHttps { .. }), "{err}");
    }

    #[test]
    fn more_than_three_redirects_end_the_request() {
        let stub = HttpStub::start_with_self(|request, me| {
            Response::redirect(302, &format!("{me}/next-{}", request.path.len()))
        });
        let (http, _dir) = client(&stub.url("/"));
        let err = http.get(&Url::parse(&stub.url("/a")).unwrap()).unwrap_err();
        assert_eq!(err, NetError::TooManyRedirects);
        // Three redirects are followed: the fourth answer ends it.
        assert_eq!(stub.requests().len(), 4);
        // Three are followed.
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&count);
        let stub = HttpStub::start_with_self(move |_, me| {
            if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 {
                Response::redirect(302, &format!("{me}/again"))
            } else {
                Response::json(200, "{}")
            }
        });
        let (http, _dir) = client(&stub.url("/"));
        assert_eq!(
            http.get(&Url::parse(&stub.url("/a")).unwrap())
                .unwrap()
                .status,
            200
        );
    }

    #[test]
    fn a_redirect_without_a_usable_location_is_refused() {
        let stub = HttpStub::start(|_| Response {
            status: 302,
            headers: vec![],
            body: vec![],
        });
        let (http, _dir) = client(&stub.url("/"));
        assert!(matches!(
            http.get(&Url::parse(&stub.url("/a")).unwrap()),
            Err(NetError::BadRedirect { .. })
        ));
        let stub = HttpStub::start(|_| Response::redirect(302, "//evil.example/x"));
        let (http, _dir) = client(&stub.url("/"));
        assert!(matches!(
            http.get(&Url::parse(&stub.url("/a")).unwrap()),
            Err(NetError::BadRedirect { .. })
        ));
    }

    #[test]
    fn a_post_that_is_redirected_is_not_followed() {
        let target = HttpStub::start(|_| Response::json(200, "{}"));
        let target_url = target.url("/x");
        let stub = HttpStub::start(move |_| Response::redirect(307, &target_url));
        let (http, _dir) = client(&stub.url("/"));
        let err = http
            .post(
                &Url::parse(&stub.url("/token")).unwrap(),
                "application/json",
                None,
                b"{}",
            )
            .unwrap_err();
        assert!(matches!(err, NetError::Redirected { .. }), "{err}");
        assert!(target.requests().is_empty());
    }

    #[test]
    fn a_reply_over_64_kib_is_too_large() {
        let stub = HttpStub::start(|_| Response::json(200, &"x".repeat(MAX_DOCUMENT_BYTES + 10)));
        let (http, _dir) = client(&stub.url("/"));
        let err = http
            .get(&Url::parse(&stub.url("/big")).unwrap())
            .unwrap_err();
        assert!(matches!(err, NetError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn a_cancelled_flow_makes_no_request() {
        let stub = HttpStub::start(|_| Response::json(200, "{}"));
        let url = Url::parse(&stub.url("/")).unwrap();
        let cancel = CancellationToken::new();
        let http = GatedHttp::for_server(&url, None, None, cancel.clone());
        cancel.cancel();
        assert_eq!(http.get(&url).unwrap_err(), NetError::Cancelled);
        assert!(stub.requests().is_empty());
    }

    #[test]
    fn an_unreachable_host_is_named_and_nothing_else_is() {
        // A port nothing listens on.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let http = GatedHttp::for_server(&url, None, None, CancellationToken::new());
        let err = http.get(&url).unwrap_err();
        assert!(matches!(err, NetError::Unreachable { .. }), "{err}");
        assert!(err.to_string().contains(&format!("127.0.0.1:{port}")));
    }
}
