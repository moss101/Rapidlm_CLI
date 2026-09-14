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

use std::time::Duration;

use capability_broker::CancellationToken;
use llm_router::providers::openai_compatible::{Http1Transport, StaticWireAuth};
use mcp::transport::{HttpRequest, HttpResponse, StreamableHttpIo, TransportError};
use security::ConsumedConnect;

/// MCP HTTP io over llm-router's blocking HTTP/1.1 + TLS client.
pub struct RapidHttpIo {
    /// Extra headers from the server's config entry (auth, tenant routing),
    /// attached verbatim to every exchange.
    pub headers: Vec<(String, String)>,
    /// Bearer token for `Authorization`, when the config names one.
    pub bearer: Option<String>,
}

impl StreamableHttpIo for RapidHttpIo {
    fn exchange(
        &mut self,
        request: &HttpRequest<'_>,
        _grant: &ConsumedConnect,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        let token = self.bearer.clone().unwrap_or_default();
        if token.is_empty() {
            // llm-router's StaticWireAuth refuses empty tokens; a no-auth
            // server still needs a well-formed header set, so a placeholder
            // credential keeps the client honest (the header is dropped
            // below for empty tokens).
        }
        let transport = Http1Transport::with_limits(
            StaticWireAuth::bearer(if token.is_empty() {
                "anonymous".to_owned()
            } else {
                token.clone()
            })
            .map_err(|err| TransportError::HandshakeFailed)?,
            timeout,
            256 * 1024,
        );

        let mut headers: Vec<(String, String)> = vec![
            ("Accept".to_owned(), "application/json".to_owned()),
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
            headers.push((name.clone(), value.clone()));
        }
        if !token.is_empty() {
            headers.push(("Authorization".to_owned(), format!("Bearer {token}")));
        }

        // Bridge the capability-broker token into the router's (a watcher
        // cancels the router token the moment the broker's fires), the same
        // pattern `model.rs`'s ProviderCancelWatch uses.
        let router_cancel = llm_router::provider::CancellationToken::new();
        let router_for_watch = router_cancel.clone();
        let cancel_for_watch = cancel.clone();
        std::thread::spawn(move || {
            while !cancel_for_watch.is_cancelled() && !router_for_watch.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            router_for_watch.cancel();
        });
        let response = transport
            .post_raw(
                &request.url(),
                &headers,
                request.body(),
                &token,
                &router_cancel,
            )
            .map_err(|err| map_provider_error(err))?;

        // Map the raw response back into the transport's typed shape: the
        // session id and redirect location come from headers only.
        let session_id = response.header("mcp-session-id").map(str::to_owned);
        let location = response.header("location").map(str::to_owned);
        HttpResponse::new(response.status, session_id, location, response.body)
    }
}

fn map_provider_error(err: llm_router::provider::ProviderError) -> TransportError {
    use llm_router::provider::ProviderError as P;
    match err {
        P::Cancelled => TransportError::Cancelled,
        P::Connection => TransportError::HandshakeFailed,
        _ => TransportError::HandshakeFailed,
    }
}
