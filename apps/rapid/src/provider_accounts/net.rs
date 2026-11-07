//! Bounded account grants over shared HTTP/TLS/proxy and exact endpoint gates.
pub use super::GrantEndpoint;
use super::{
    AccountError as Error, AccountKind, AccountSpec, MAX_RECORD_BYTES, protocol::GrantBody,
};
use crate::mcp_oauth::Url;
use crate::provider_egress::ProviderEgress;
use llm_router::providers::{
    dial::{DialGate, DialTarget, ProxyConfig, ProxyTarget},
    openai_compatible::{Http1Transport, StaticWireAuth},
};
use llm_router::{CancellationToken, ProviderError};
use std::{net::SocketAddr, sync::Arc, time::Duration};
pub struct GrantReply {
    pub status: u16,
    pub body: Vec<u8>,
}
impl std::fmt::Debug for GrantReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantReply")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}
impl Drop for GrantReply {
    fn drop(&mut self) {
        self.body.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}
pub trait GrantTransport: Send + Sync {
    fn post(
        &self,
        spec: &AccountSpec,
        endpoint: GrantEndpoint,
        body: &GrantBody,
        cancel: &CancellationToken,
    ) -> Result<GrantReply, Error>;
}
#[derive(Clone, Default)]
pub struct AccountHttp {
    proxy: Option<ProxyConfig>,
    gate: Option<Arc<dyn DialGate>>,
    kimi_identity: Option<KimiDeviceIdentity>,
}
/// Explicit, caller-provided identity for Kimi's OAuth host. Constructing this
/// value does not inspect the machine or create a persistent device identifier.
#[derive(Clone)]
pub struct KimiDeviceIdentity {
    device_id: String,
    device_name: String,
    device_model: String,
    os_version: String,
}
impl std::fmt::Debug for KimiDeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KimiDeviceIdentity(<redacted>)")
    }
}
impl KimiDeviceIdentity {
    pub fn inference_values(&self) -> [String; 5] {
        [
            env!("CARGO_PKG_VERSION").to_owned(),
            self.device_name.clone(),
            self.device_model.clone(),
            self.os_version.clone(),
            self.device_id.clone(),
        ]
    }
    pub fn new(
        device_id: String,
        device_name: String,
        device_model: String,
        os_version: String,
    ) -> Result<Self, Error> {
        if [&device_id, &device_name, &device_model, &os_version]
            .iter()
            .any(|value| {
                value.is_empty()
                    || value.len() > 256
                    || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
            })
        {
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            device_id,
            device_name,
            device_model,
            os_version,
        })
    }
}
impl AccountHttp {
    pub fn new(proxy: Option<ProxyConfig>, gate: Option<Arc<dyn DialGate>>) -> Self {
        Self {
            proxy,
            gate,
            kimi_identity: None,
        }
    }
    pub fn with_kimi_identity(mut self, identity: KimiDeviceIdentity) -> Self {
        self.kimi_identity = Some(identity);
        self
    }
}
struct ScopedGate {
    extra: Option<Arc<dyn DialGate>>,
    exact: ProviderEgress,
}
impl DialGate for ScopedGate {
    fn permit(
        &self,
        target: DialTarget<'_>,
        via: Option<&ProxyTarget>,
        addrs: &[SocketAddr],
    ) -> Result<Vec<SocketAddr>, ProviderError> {
        let resolved = match &self.extra {
            Some(gate) => gate.permit(target, via, addrs)?,
            None => addrs.to_vec(),
        };
        self.exact.permit(target, via, &resolved)
    }
}
impl GrantTransport for AccountHttp {
    fn post(
        &self,
        spec: &AccountSpec,
        role: GrantEndpoint,
        body: &GrantBody,
        cancel: &CancellationToken,
    ) -> Result<GrantReply, Error> {
        cancel.check().map_err(|_| Error::Cancelled)?;
        if !body.matches(spec, role) {
            return Err(Error::BindingMismatch);
        }
        let d = spec.definition();
        let text = match role {
            GrantEndpoint::Token => d.token_url.as_str(),
            GrantEndpoint::Device => d.device_url.as_deref().ok_or(Error::InvalidConfig)?,
            GrantEndpoint::Revoke => d.revoke_url.as_deref().ok_or(Error::InvalidConfig)?,
        };
        let url = Url::parse(text).map_err(|_| Error::InvalidConfig)?;
        let via = self
            .proxy
            .as_ref()
            .and_then(|p| p.for_target(url.is_https(), url.host(), url.port()))
            .map(|p| (p.host(), p.port()));
        let exact = ProviderEgress::for_endpoint(url.is_https(), url.host(), url.port(), via)
            .map_err(|_| Error::InvalidConfig)?;
        let mut transport = Http1Transport::with_limits(
            StaticWireAuth::bearer("account-public-client").map_err(|_| Error::InvalidConfig)?,
            Duration::from_secs(10),
            MAX_RECORD_BYTES,
        )
        .with_dial_gate(Arc::new(ScopedGate {
            extra: self.gate.clone(),
            exact,
        }));
        if let Some(proxy) = &self.proxy {
            transport = transport.with_proxy(proxy.clone());
        }
        let mut headers = vec![
            ("Content-Type".to_owned(), body.content_type().to_owned()),
            ("Accept".to_owned(), "application/json".to_owned()),
            (
                "User-Agent".to_owned(),
                format!("RapidLM/{}", env!("CARGO_PKG_VERSION")),
            ),
        ];
        if d.kind == AccountKind::Kimi {
            let identity = self.kimi_identity.as_ref().ok_or(Error::InvalidConfig)?;
            headers.extend([
                ("X-Msh-Platform".to_owned(), "rapidlm_cli".to_owned()),
                (
                    "X-Msh-Version".to_owned(),
                    env!("CARGO_PKG_VERSION").to_owned(),
                ),
                ("X-Msh-Device-Name".to_owned(), identity.device_name.clone()),
                (
                    "X-Msh-Device-Model".to_owned(),
                    identity.device_model.clone(),
                ),
                ("X-Msh-Os-Version".to_owned(), identity.os_version.clone()),
                ("X-Msh-Device-Id".to_owned(), identity.device_id.clone()),
            ]);
        }
        let reply = transport
            .post_raw_authorized(text, &headers, body.bytes(), None, cancel)
            .map_err(|e| match e {
                ProviderError::Cancelled => Error::Cancelled,
                ProviderError::InvalidRequest => Error::InvalidConfig,
                _ => Error::Transient,
            })?;
        // No redirect is followed, even within the issuer's own origin.
        if (300..400).contains(&reply.status) {
            return Err(Error::InvalidReply);
        }
        Ok(GrantReply {
            status: reply.status,
            body: reply.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_oauth::Secret;
    use crate::provider_accounts::{
        protocol::{Tokens, refresh_request},
        tests::spec,
    };
    use std::io::{Read, Write};
    fn request(spec: &AccountSpec) -> GrantBody {
        refresh_request(
            spec,
            &Tokens {
                access: Secret::new("access-canary".to_owned()),
                refresh: Some(Secret::new("refresh-canary".to_owned())),
                obtained_at: 1,
                expires_at: Some(100),
                scopes: vec![],
            },
        )
        .unwrap()
    }
    #[test]
    fn account_http_sends_a_bounded_grant_only_to_its_selected_endpoint_with_no_ambient_authorization()
     {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                let raw = String::from_utf8_lossy(&bytes);
                if let Some((head, body)) = raw.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Content-Length: ")
                                .and_then(|n| n.parse::<usize>().ok())
                        })
                        .unwrap();
                    if body.len() >= len {
                        break;
                    }
                }
            }
            let raw = String::from_utf8(bytes).unwrap();
            assert!(raw.starts_with("POST /token "));
            let (head, body) = raw.split_once("\r\n\r\n").unwrap();
            assert!(!head.to_ascii_lowercase().contains("authorization:"));
            assert!(head.contains("application/x-www-form-urlencoded"));
            assert!(body.contains("refresh_token=refresh-canary"));
            let reply = b"{\"access_token\":\"new-canary\",\"expires_in\":3600}";
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            )
            .unwrap();
            socket.write_all(reply).unwrap();
        });
        let mut def = spec().definition().clone();
        def.token_url = format!("http://{addr}/token");
        let spec = AccountSpec::new("work", def).unwrap();
        let reply = AccountHttp::default()
            .post(
                &spec,
                GrantEndpoint::Token,
                &request(&spec),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(reply.status, 200);
        assert!(!format!("{reply:?}").contains("new-canary"));
        worker.join().unwrap();
    }
    #[test]
    fn account_http_egress_refusal_happens_before_a_grant_is_sent() {
        struct Refuse;
        impl DialGate for Refuse {
            fn permit(
                &self,
                _: DialTarget<'_>,
                _: Option<&ProxyTarget>,
                _: &[SocketAddr],
            ) -> Result<Vec<SocketAddr>, ProviderError> {
                Err(ProviderError::Connection)
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut def = spec().definition().clone();
        def.token_url = format!("http://{}/token", listener.local_addr().unwrap());
        let spec = AccountSpec::new("work", def).unwrap();
        assert_eq!(
            AccountHttp::new(None, Some(Arc::new(Refuse)))
                .post(
                    &spec,
                    GrantEndpoint::Token,
                    &request(&spec),
                    &CancellationToken::new()
                )
                .unwrap_err(),
            Error::Transient
        );
        assert!(listener.accept().is_err());
    }
    #[test]
    fn kimi_grants_require_explicit_bounded_rapidlm_identity() {
        use crate::provider_accounts::{AccountFlow, protocol::device_request};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut def = spec().definition().clone();
        def.kind = AccountKind::Kimi;
        def.flow = AccountFlow::Device;
        def.authorize_url = None;
        def.redirect_url = None;
        def.device_url = Some(format!("http://{addr}/device"));
        let spec = AccountSpec::new("work", def).unwrap();
        let body = device_request(&spec).unwrap();
        assert_eq!(
            AccountHttp::default()
                .post(
                    &spec,
                    GrantEndpoint::Device,
                    &body,
                    &CancellationToken::new()
                )
                .unwrap_err(),
            Error::InvalidConfig
        );
        assert!(
            KimiDeviceIdentity::new(
                "id\r\nInjected: yes".into(),
                "host".into(),
                "model".into(),
                "os".into()
            )
            .is_err()
        );
        let identity = KimiDeviceIdentity::new(
            "device-canary".into(),
            "test-host".into(),
            "test-model".into(),
            "test-os".into(),
        )
        .unwrap();
        assert!(!format!("{identity:?}").contains("device-canary"));
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
            assert!(head.contains("x-msh-platform: rapidlm_cli\r\n"));
            assert!(head.contains("x-msh-device-id: device-canary\r\n"));
            assert!(head.contains("x-msh-device-name: test-host\r\n"));
            assert!(!head.contains("authorization:"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
        });
        let response = AccountHttp::default()
            .with_kimi_identity(identity)
            .post(
                &spec,
                GrantEndpoint::Device,
                &body,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(response.status, 200);
        worker.join().unwrap();
    }
}
