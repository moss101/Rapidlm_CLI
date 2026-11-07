//! AWS SigV4 header authentication. Uses ring HMAC/SHA256 and explicit wire input.
//! Secret material is transient; Debug never includes credentials or signed headers.
use crate::ProviderError;
use ring::{digest, hmac};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone)]
pub struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}
impl AwsCredentials {
    pub fn new(
        access_key_id: &str,
        secret_access_key: &str,
        session_token: Option<&str>,
    ) -> Result<Self, ProviderError> {
        if access_key_id.is_empty()
            || access_key_id.len() > 128
            || !access_key_id.bytes().all(|b| b.is_ascii_alphanumeric())
            || secret_access_key.is_empty()
            || secret_access_key.len() > 256
            || !secret_access_key
                .bytes()
                .all(|b| (0x21..=0x7e).contains(&b))
            || session_token.is_some_and(|t| {
                t.is_empty() || t.len() > 8192 || !t.bytes().all(|b| (0x21..=0x7e).contains(&b))
            })
        {
            return Err(ProviderError::AuthFailed);
        }
        Ok(Self {
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            session_token: session_token.map(str::to_owned),
        })
    }
}
impl fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AwsCredentials(<private>)")
    }
}
fn wipe(text: &mut String) {
    let mut bytes = std::mem::take(text).into_bytes();
    bytes.fill(0);
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}
impl Drop for AwsCredentials {
    fn drop(&mut self) {
        wipe(&mut self.access_key_id);
        wipe(&mut self.secret_access_key);
        if let Some(token) = &mut self.session_token {
            wipe(token);
        }
    }
}

/// Exactly the origin host and target that the transport writes, plus adapter headers/body.
/// The transport supplies these after URL validation and egress planning.
pub struct SignableRequest<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path_and_query: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    pub payload_hash_header: bool,
}
impl fmt::Debug for SignableRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignableRequest(<wire data>)")
    }
}
/// Signed fields materialized only at the wire. No unrestricted header names.
pub struct AwsAuthorization {
    authorization: String,
    amz_date: String,
    session_token: Option<String>,
    payload_hash: Option<String>,
}
impl fmt::Debug for AwsAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AwsAuthorization(<private>)")
    }
}
impl AwsAuthorization {
    pub fn authorization(&self) -> &str {
        &self.authorization
    }
    pub fn amz_date(&self) -> &str {
        &self.amz_date
    }
    pub fn payload_hash(&self) -> Option<&str> {
        self.payload_hash.as_deref()
    }
    pub fn session_token(&self) -> Option<&str> {
        self.session_token.as_deref()
    }
}
impl Drop for AwsAuthorization {
    fn drop(&mut self) {
        wipe(&mut self.authorization);
        if let Some(token) = &mut self.session_token {
            wipe(token);
        }
    }
}

pub fn sign(
    request: &SignableRequest<'_>,
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    amz_date: &str,
) -> Result<AwsAuthorization, ProviderError> {
    let (canonical, signed_headers) = canonical_request(request, credentials, amz_date)?;
    if !scope_label(region) || !scope_label(service) {
        return Err(ProviderError::InvalidRequest);
    }
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256(canonical.as_bytes())
    );
    let mut initial = format!("AWS4{}", credentials.secret_access_key).into_bytes();
    let kdate = mac(&initial, date.as_bytes());
    initial.fill(0);
    let kregion = mac(kdate.as_ref(), region.as_bytes());
    let kservice = mac(kregion.as_ref(), service.as_bytes());
    let ksign = mac(kservice.as_ref(), b"aws4_request");
    let signature = hex(mac(ksign.as_ref(), string_to_sign.as_bytes()).as_ref());
    Ok(AwsAuthorization {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
        amz_date: amz_date.to_owned(),
        session_token: credentials.session_token.clone(),
        payload_hash: request.payload_hash_header.then(|| sha256(request.body)),
    })
}
fn scope_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 64
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn mac(key: &[u8], value: &[u8]) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), value)
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}
fn sha256(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}
/// RFC3986 encoding used for resource path segments and canonical URI/query values.
pub fn uri_encode(bytes: &[u8], keep_slashes: bool) -> String {
    const DIGITS: &[u8] = b"0123456789ABCDEF";
    let mut out = String::new();
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (keep_slashes && b == b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(DIGITS[(b >> 4) as usize] as char);
            out.push(DIGITS[(b & 15) as usize] as char);
        }
    }
    out
}
fn percent_decode(bytes: &[u8]) -> Result<Vec<u8>, ProviderError> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = *bytes.get(i + 1).ok_or(ProviderError::InvalidRequest)?;
            let lo = *bytes.get(i + 2).ok_or(ProviderError::InvalidRequest)?;
            out.push(
                digit(hi).ok_or(ProviderError::InvalidRequest)? * 16
                    + digit(lo).ok_or(ProviderError::InvalidRequest)?,
            );
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}
fn canonical_request(
    request: &SignableRequest<'_>,
    credentials: &AwsCredentials,
    amz_date: &str,
) -> Result<(String, String), ProviderError> {
    if amz_date.len() != 16
        || amz_date.as_bytes()[8] != b'T'
        || amz_date.as_bytes()[15] != b'Z'
        || !amz_date
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 8 || i == 15 || b.is_ascii_digit())
        || request.method.is_empty()
        || request.method.len() > 16
        || !request.method.bytes().all(|b| b.is_ascii_uppercase())
        || request.host.is_empty()
        || request.host.len() > 512
        || request
            .host
            .bytes()
            .any(|b| !b.is_ascii_graphic() || b"/@?#\\".contains(&b))
        || !request.path_and_query.starts_with('/')
        || request.path_and_query.len() > 8192
        || request.path_and_query.chars().any(char::is_control)
        || request.path_and_query.contains('#')
        || request.body.len() > super::openai_compatible::MAX_HTTP_REQUEST_BYTES
        || request.headers.len() > 64
    {
        return Err(ProviderError::InvalidRequest);
    }
    let (path, query) = request
        .path_and_query
        .split_once('?')
        .unwrap_or((request.path_and_query, ""));
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut normalized = format!("/{}", parts.join("/"));
    if path.ends_with('/') && !normalized.ends_with('/') {
        normalized.push('/');
    }
    let canonical_uri = uri_encode(normalized.as_bytes(), true);
    let mut pairs = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        pairs.push((
            uri_encode(&percent_decode(k.as_bytes())?, false),
            uri_encode(&percent_decode(v.as_bytes())?, false),
        ));
    }
    pairs.sort();
    let canonical_query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut headers = BTreeMap::<String, String>::new();
    for (name, value) in request.headers {
        let name = name.to_ascii_lowercase();
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || name == "authorization"
            || value.len() > 8192
            || value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f)
        {
            return Err(ProviderError::InvalidRequest);
        }
        // Transport-generated/hop-by-hop headers must not become brittle signature inputs.
        if matches!(
            name.as_str(),
            "user-agent" | "connection" | "transfer-encoding" | "expect" | "x-amzn-trace-id"
        ) {
            continue;
        }
        let value = value.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
        headers
            .entry(name)
            .and_modify(|prior| {
                prior.push(',');
                prior.push_str(&value)
            })
            .or_insert(value);
    }
    if headers.get("host").is_some_and(|v| v != request.host)
        || headers.get("x-amz-date").is_some_and(|v| v != amz_date)
        || headers
            .get("x-amz-security-token")
            .is_some_and(|v| credentials.session_token.as_ref() != Some(v))
    {
        return Err(ProviderError::InvalidRequest);
    }
    headers.insert("host".to_owned(), request.host.to_owned());
    headers.insert("x-amz-date".to_owned(), amz_date.to_owned());
    if let Some(token) = &credentials.session_token {
        headers.insert("x-amz-security-token".to_owned(), token.clone());
    }
    if request.payload_hash_header {
        let hash = sha256(request.body);
        if headers
            .get("x-amz-content-sha256")
            .is_some_and(|value| value != &hash)
        {
            return Err(ProviderError::InvalidRequest);
        }
        headers.insert("x-amz-content-sha256".to_owned(), hash);
    }
    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    Ok((
        format!(
            "{}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{}",
            request.method,
            sha256(request.body)
        ),
        signed_headers,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aws_sigv4_matches_official_aws_c_auth_header_signing_vectors() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/aws-signing/official-vectors.json"
        ))
        .unwrap();
        for (name, case) in cases.as_object().unwrap() {
            let context = &case["context"];
            let text = case["request"].as_str().unwrap();
            let (head, body) = text.split_once("\n\n").unwrap_or((text, ""));
            let mut lines = head.lines();
            let mut first = lines.next().unwrap().split_whitespace();
            let method = first.next().unwrap();
            let target = first.next().unwrap();
            let headers = lines
                .filter(|l| !l.is_empty())
                .map(|l| {
                    let (k, v) = l.split_once(':').unwrap();
                    (k.to_owned(), v.to_owned())
                })
                .collect::<Vec<_>>();
            let host = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("host"))
                .unwrap()
                .1
                .trim();
            let input = SignableRequest {
                method,
                host,
                path_and_query: target,
                headers: &headers,
                body: body.as_bytes(),
                payload_hash_header: context["sign_body"].as_bool().unwrap(),
            };
            let credentials = AwsCredentials::new(
                context["credentials"]["access_key_id"].as_str().unwrap(),
                context["credentials"]["secret_access_key"]
                    .as_str()
                    .unwrap(),
                context["credentials"]
                    .get("token")
                    .and_then(serde_json::Value::as_str),
            )
            .unwrap();
            let timestamp = context["timestamp"]
                .as_str()
                .unwrap()
                .replace(['-', ':'], "");
            assert_eq!(
                canonical_request(&input, &credentials, &timestamp)
                    .unwrap()
                    .0,
                case["canonical"].as_str().unwrap(),
                "{name} canonical"
            );
            let result = sign(
                &input,
                &credentials,
                context["region"].as_str().unwrap(),
                context["service"].as_str().unwrap(),
                &timestamp,
            )
            .unwrap();
            assert!(
                result
                    .authorization()
                    .ends_with(case["signature"].as_str().unwrap()),
                "{name} signature"
            );
        }
    }
    #[test]
    fn aws_sigv4_binds_session_region_host_body_and_encoded_resource_without_leaking_debug() {
        let credentials =
            AwsCredentials::new("AKIDEXAMPLE", "private-secret", Some("private-session")).unwrap();
        let headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        let input = SignableRequest {
            method: "POST",
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path_and_query: "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Ftest/converse-stream",
            headers: &headers,
            body: b"{}",
            payload_hash_header: true,
        };
        let first = sign(
            &input,
            &credentials,
            "us-east-1",
            "bedrock",
            "20261026T120000Z",
        )
        .unwrap();
        assert!(
            first
                .authorization()
                .contains("/us-east-1/bedrock/aws4_request")
        );
        assert_eq!(first.session_token(), Some("private-session"));
        assert!(first.authorization().contains("x-amz-security-token"));
        assert!(!format!("{credentials:?}{first:?}{input:?}").contains("private-"));
        assert_ne!(
            first.authorization(),
            sign(
                &input,
                &credentials,
                "eu-west-1",
                "bedrock",
                "20261026T120000Z"
            )
            .unwrap()
            .authorization()
        );
        let other = SignableRequest {
            body: b"{\"different\":true}",
            ..input
        };
        assert_ne!(
            first.authorization(),
            sign(
                &other,
                &credentials,
                "us-east-1",
                "bedrock",
                "20261026T120000Z"
            )
            .unwrap()
            .authorization()
        );
        assert!(
            canonical_request(&input, &credentials, "20261026T120000Z")
                .unwrap()
                .0
                .contains("arn%253Aaws%253A")
        );
        assert!(
            sign(
                &input,
                &credentials,
                "bad/region",
                "bedrock",
                "20261026T120000Z"
            )
            .is_err()
        );
        assert!(sign(&input, &credentials, "us-east-1", "bedrock", "bad").is_err());
        assert!(AwsCredentials::new("access", "bad\nsecret", None).is_err());
    }
}
