//! `rapid mcp auth`'s device flow: the interactive half of `mcp_oauth`.
//!
//! One terminal-shaped function: fetch discovery, POST the device
//! authorization, print the verification URL and user code, poll until the
//! token arrives (the flow's own `interval` and `expires_in` pace and bound
//! it), store the token beside the project's settings, and say what to do
//! next. Every failure is a line naming the step that failed — a flow the
//! user can re-run, never a half-state left on disk.

use std::io::Write as _;
use std::time::{Duration, Instant};

use crate::mcp_oauth::{
    AuthorizationServer, DeviceAuthorization, StoredToken, TokenPoll, discovery_url,
    parse_device_authorization, parse_discovery, parse_token_response,
};

/// How long one HTTP exchange may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// The whole flow, printing as it goes (stdout for the user's steps,
/// stderr for progress). Returns the success line, or the failure named.
pub fn run_device_flow(endpoint: &str, server: &str) -> Result<String, String> {
    let authorization = discover(endpoint)?;
    let device = request_device_authorization(&authorization)?;
    println!("open {} and enter the code:", device.verification_uri);
    println!("  {}", device.user_code);
    eprintln!("polling every {}s until you finish…", device.interval_secs);
    let token = poll_for_token(&authorization, &device)?;
    let marker = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::interactive::resolve_project_root(&cwd, &kernel_cancel()).ok())
        .map(|found| found.root.join(crate::interactive::PROJECT_MARKER))
        .ok_or_else(|| "the project root could not be resolved to store the token".to_owned())?;
    crate::mcp_oauth::store_token(
        &marker,
        server,
        &StoredToken {
            access_token: token.0,
            refresh_token: token.1,
        },
    )?;
    let _ = std::io::stdout().flush();
    Ok(format!(
        "authorized {server}; its token is stored and every later connect sends it"
    ))
}

fn kernel_cancel() -> kernel::CancellationToken {
    kernel::CancellationToken::new()
}

/// Fetch and parse the authorization-server metadata for an MCP endpoint.
fn discover(endpoint: &str) -> Result<AuthorizationServer, String> {
    let url = discovery_url(endpoint)?;
    let body = http_get(&url).map_err(|err| format!("discovery failed: {err}"))?;
    parse_discovery(&body).map_err(|err| format!("discovery: {err}"))
}

/// POST the device-authorization request.
fn request_device_authorization(
    authorization: &AuthorizationServer,
) -> Result<DeviceAuthorization, String> {
    let body = http_post_form(
        &authorization.device_authorization_endpoint,
        "client_id=rapidlm",
    )
    .map_err(|err| format!("the device authorization request failed: {err}"))?;
    parse_device_authorization(&body)
        .map_err(|err| format!("the device authorization response: {err}"))
}

/// Poll the token endpoint at the flow's own pace until it grants, the
/// code expires, or ten minutes pass — whichever comes first.
fn poll_for_token(
    authorization: &AuthorizationServer,
    device: &DeviceAuthorization,
) -> Result<(String, Option<String>), String> {
    let started = Instant::now();
    let deadline = started + crate::mcp_oauth::FLOW_TIMEOUT;
    loop {
        std::thread::sleep(Duration::from_secs(device.interval_secs.max(1)));
        if Instant::now() >= deadline {
            return Err("the flow timed out".to_owned());
        }
        if started.elapsed().as_secs() >= device.expires_in {
            return Err(
                "the code expired before you finished; run rapid mcp auth again".to_owned(),
            );
        }
        let form = format!(
            "grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code={}&client_id=rapidlm",
            device.device_code
        );
        let body = match http_post_form(&authorization.token_endpoint, &form) {
            Ok(body) => body,
            // A transport error mid-poll is one lost poll, not a dead
            // flow: the next interval tries again.
            Err(err) => {
                eprintln!("poll: {err}");
                continue;
            }
        };
        match parse_token_response(&body) {
            Ok(TokenPoll::Granted {
                access_token,
                refresh_token,
            }) => return Ok((access_token, refresh_token)),
            Ok(TokenPoll::Pending) => continue,
            Ok(TokenPoll::Denied(reason)) => {
                return Err(format!("the server denied the flow: {reason}"));
            }
            Err(reason) => return Err(format!("the token response could not be read: {reason}")),
        }
    }
}

/// One GET through the audited HTTP/1.1 transport, authorized for exactly
/// the URL's own origin (the egress gate the provider dial uses).
fn http_get(url: &str) -> Result<String, String> {
    let response = transport_for(url)?
        .get_raw(
            url,
            &[("Accept".to_owned(), "application/json".to_owned())],
            &llm_router::provider::CancellationToken::new(),
        )
        .map_err(|err| err.to_string())?;
    if !(200..300).contains(&response.status) {
        return Err(format!("answered {}", response.status));
    }
    Ok(String::from_utf8_lossy(&response.body).into_owned())
}

/// One form POST through the same transport.
fn http_post_form(url: &str, form: &str) -> Result<String, String> {
    let transport = transport_for(url)?;
    let response = transport
        .post_raw(
            url,
            &[
                (
                    "Content-Type".to_owned(),
                    "application/x-www-form-urlencoded".to_owned(),
                ),
                ("Accept".to_owned(), "application/json".to_owned()),
            ],
            form.as_bytes(),
            "rapidlm-mcp-auth",
            &llm_router::provider::CancellationToken::new(),
        )
        .map_err(|err| err.to_string())?;
    if !(200..300).contains(&response.status) {
        return Err(format!("answered {}", response.status));
    }
    Ok(String::from_utf8_lossy(&response.body).into_owned())
}

fn transport_for(
    url: &str,
) -> Result<
    llm_router::providers::openai_compatible::Http1Transport<
        llm_router::providers::openai_compatible::StaticWireAuth,
    >,
    String,
> {
    let https = url.starts_with("https://");
    let host = crate::web_fetch::host_of(url)
        .ok_or_else(|| "the url has no host".to_owned())?
        .to_ascii_lowercase();
    let port = {
        let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        authority
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .unwrap_or(if https { 443 } else { 80 })
    };
    let gate = std::sync::Arc::new(
        crate::provider_egress::ProviderEgress::for_client(
            security::NetworkClient::Tool,
            https,
            &host,
            port,
            None,
        )
        .map_err(|err| format!("egress gate: {err}"))?,
    );
    let auth = llm_router::providers::openai_compatible::StaticWireAuth::bearer(
        "rapidlm-mcp-auth".to_owned(),
    )
    .map_err(|err| err.to_string())?;
    Ok(
        llm_router::providers::openai_compatible::Http1Transport::with_limits(
            auth,
            HTTP_TIMEOUT,
            crate::mcp_oauth::MAX_DISCOVERY_BYTES,
        )
        .with_dial_gate(std::sync::Arc::clone(&gate)
            as std::sync::Arc<dyn llm_router::providers::dial::DialGate>),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flow_paces_itself_and_expires_when_the_code_does() {
        // Pure pacing: the poll helper's bounds are the flow's own — the
        // transport itself is exercised by mcp_oauth's parsers; here the
        // deadline math is what's under test.
        let authorization = AuthorizationServer {
            issuer: "https://sso".to_owned(),
            device_authorization_endpoint: "https://sso/d".to_owned(),
            token_endpoint: "https://sso/t".to_owned(),
        };
        // A device code that expired before the first poll: the flow says
        // expired, not timeout.
        let expired = DeviceAuthorization {
            device_code: "DC".to_owned(),
            user_code: "U".to_owned(),
            verification_uri: "https://sso/v".to_owned(),
            interval_secs: 1,
            expires_in: 0,
        };
        // The sleep happens before the expiry check, so a zero-lived code
        // expires on the first loop iteration.
        let err = poll_for_token(&authorization, &expired).expect_err("expired");
        assert!(err.contains("expired"), "{err}");
    }
}
