//! Controller↔worker mTLS identity binding.
//!
//! Worker ID is taken from the presented leaf certificate SAN, never from a
//! worker payload. Configured CAs and/or pinned fingerprints are required;
//! empty trust fails closed. Threat `T-010`.

use std::fmt::{self, Debug, Display, Formatter};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{ControllerId, WorkerId};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, TrustAnchor, UnixTime};
use sha2::{Digest, Sha256};
use webpki::{EndEntityCert, Error as WebPkiError, KeyUsage, anchor_from_trusted_cert};

use crate::store::CancellationToken;

/// Maximum DER bytes accepted for one certificate.
pub const MAX_CERT_DER_BYTES: usize = 16 * 1024;

/// Maximum intermediate certificates presented with a leaf.
pub const MAX_INTERMEDIATES: usize = 8;

/// Maximum configured CA certificates.
pub const MAX_CA_CERTS: usize = 16;

/// Maximum configured certificate pins.
pub const MAX_PINNED_IDENTITIES: usize = 64;

/// SHA-256 fingerprint length.
pub const CERT_FINGERPRINT_LEN: usize = 32;

const WORKER_URI_PREFIX: &str = "urn:rapidlm:worker:";
const CONTROLLER_URI_PREFIX: &str = "urn:rapidlm:controller:";
const HEX_TABLE: &[u8; 16] = b"0123456789abcdef";

/// Typed mTLS identity failures. Display never includes certificate or key bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MtlsError {
    Cancelled,
    EmptyTrust,
    Untrusted,
    Expired,
    NotYetValid,
    IdentityMismatch,
    IdentityMissing,
    PinMismatch,
    InvalidCertificate,
    InvalidTime,
    BoundExceeded { limit: usize, requested: usize },
}

/// SHA-256 digest of a presented leaf certificate DER.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct CertFingerprint([u8; CERT_FINGERPRINT_LEN]);

/// Configured controller↔worker trust: CAs and/or exact leaf pins.
///
/// At least one CA or one pin is required. When both are configured, the leaf
/// must chain to a configured CA **and** match a pin. Pins never broaden CA
/// trust.
#[derive(Clone, Eq, PartialEq)]
pub struct MtlsTrust {
    ca_certs: Vec<CertificateDer<'static>>,
    pins: Vec<CertFingerprint>,
}

/// Leaf plus optional intermediates presented by a TLS peer.
#[derive(Clone, Eq, PartialEq)]
pub struct PresentedCertificate {
    leaf: CertificateDer<'static>,
    intermediates: Vec<CertificateDer<'static>>,
}

/// Worker-advertised identity. Not authority; [`AuthenticatedWorker::id`] is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerClaims {
    advertised_id: Option<WorkerId>,
}

/// Connection identity bound from the worker certificate.
#[derive(Clone, Eq, PartialEq)]
pub struct AuthenticatedWorker {
    id: WorkerId,
    cert_fingerprint: CertFingerprint,
    claims: WorkerClaims,
}

/// Connection identity bound from the controller certificate.
#[derive(Clone, Eq, PartialEq)]
pub struct AuthenticatedController {
    id: ControllerId,
    cert_fingerprint: CertFingerprint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PeerRole {
    Worker,
    Controller,
}

impl MtlsError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "auth.mtls.cancelled",
            Self::EmptyTrust => "auth.mtls.empty_trust",
            Self::Untrusted => "auth.mtls.untrusted",
            Self::Expired => "auth.mtls.expired",
            Self::NotYetValid => "auth.mtls.not_yet_valid",
            Self::IdentityMismatch => "auth.mtls.identity_mismatch",
            Self::IdentityMissing => "auth.mtls.identity_missing",
            Self::PinMismatch => "auth.mtls.pin_mismatch",
            Self::InvalidCertificate => "auth.mtls.invalid_certificate",
            Self::InvalidTime => "auth.mtls.invalid_time",
            Self::BoundExceeded { .. } => "auth.mtls.bound_exceeded",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for MtlsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("mTLS identity check was cancelled"),
            Self::EmptyTrust => f.write_str("mTLS trust requires a CA or pinned identity"),
            Self::Untrusted => f.write_str("peer certificate is not trusted"),
            Self::Expired => f.write_str("peer certificate is expired"),
            Self::NotYetValid => f.write_str("peer certificate is not yet valid"),
            Self::IdentityMismatch => {
                f.write_str("worker payload cannot override certificate identity")
            }
            Self::IdentityMissing => f.write_str("peer certificate is missing a bound identity"),
            Self::PinMismatch => f.write_str("peer certificate does not match a pinned identity"),
            Self::InvalidCertificate => f.write_str("peer certificate is invalid"),
            Self::InvalidTime => f.write_str("mTLS validation time is invalid"),
            Self::BoundExceeded { limit, requested } => {
                write!(f, "mTLS bound exceeded ({requested} > {limit})")
            }
        }
    }
}

impl std::error::Error for MtlsError {}

impl CertFingerprint {
    pub fn sha256(der: &[u8]) -> Self {
        let digest = Sha256::digest(der);
        let mut bytes = [0u8; CERT_FINGERPRINT_LEN];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; CERT_FINGERPRINT_LEN]) -> Result<Self, MtlsError> {
        if bytes.iter().all(|b| *b == 0) {
            return Err(MtlsError::InvalidCertificate);
        }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; CERT_FINGERPRINT_LEN] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }
}

impl Display for CertFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Debug for CertFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CertFingerprint")
            .field(&self.to_hex())
            .finish()
    }
}

impl MtlsTrust {
    /// Build trust from CA DERs and optional leaf pins. Empty input is rejected.
    pub fn new(
        ca_ders: impl IntoIterator<Item = Vec<u8>>,
        pins: impl IntoIterator<Item = CertFingerprint>,
        cancel: &CancellationToken,
    ) -> Result<Self, MtlsError> {
        cancel.check().map_err(|_| MtlsError::Cancelled)?;
        let mut ca_certs = Vec::new();
        for der in ca_ders {
            check_cert_bound(&der)?;
            let cert = CertificateDer::from(der);
            anchor_from_trusted_cert(&cert).map_err(|_| MtlsError::InvalidCertificate)?;
            ca_certs.push(cert);
            if ca_certs.len() > MAX_CA_CERTS {
                return Err(MtlsError::BoundExceeded {
                    limit: MAX_CA_CERTS,
                    requested: ca_certs.len(),
                });
            }
        }
        let mut pins: Vec<CertFingerprint> = pins.into_iter().collect();
        if pins.len() > MAX_PINNED_IDENTITIES {
            return Err(MtlsError::BoundExceeded {
                limit: MAX_PINNED_IDENTITIES,
                requested: pins.len(),
            });
        }
        for pin in &pins {
            if pin.0.iter().all(|b| *b == 0) {
                return Err(MtlsError::InvalidCertificate);
            }
        }
        pins.sort_by_key(|pin| pin.0);
        pins.dedup();
        if ca_certs.is_empty() && pins.is_empty() {
            return Err(MtlsError::EmptyTrust);
        }
        Ok(Self { ca_certs, pins })
    }

    /// Parse one or more PEM CA certificates.
    pub fn from_ca_pem(
        pem: &[u8],
        pins: impl IntoIterator<Item = CertFingerprint>,
        cancel: &CancellationToken,
    ) -> Result<Self, MtlsError> {
        cancel.check().map_err(|_| MtlsError::Cancelled)?;
        if pem.len() > MAX_CERT_DER_BYTES.saturating_mul(MAX_CA_CERTS.saturating_add(4)) {
            return Err(MtlsError::BoundExceeded {
                limit: MAX_CERT_DER_BYTES.saturating_mul(MAX_CA_CERTS.saturating_add(4)),
                requested: pem.len(),
            });
        }
        let ders = decode_pem_certs(pem)?;
        Self::new(ders, pins, cancel)
    }

    pub fn ca_count(&self) -> usize {
        self.ca_certs.len()
    }

    pub fn pin_count(&self) -> usize {
        self.pins.len()
    }
}

impl Debug for MtlsTrust {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("MtlsTrust")
            .field("ca_count", &self.ca_certs.len())
            .field("pin_count", &self.pins.len())
            .finish()
    }
}

impl PresentedCertificate {
    pub fn from_der(
        leaf: Vec<u8>,
        intermediates: impl IntoIterator<Item = Vec<u8>>,
        cancel: &CancellationToken,
    ) -> Result<Self, MtlsError> {
        cancel.check().map_err(|_| MtlsError::Cancelled)?;
        check_cert_bound(&leaf)?;
        let mut chain = Vec::new();
        for der in intermediates {
            check_cert_bound(&der)?;
            chain.push(CertificateDer::from(der));
            if chain.len() > MAX_INTERMEDIATES {
                return Err(MtlsError::BoundExceeded {
                    limit: MAX_INTERMEDIATES,
                    requested: chain.len(),
                });
            }
        }
        Ok(Self {
            leaf: CertificateDer::from(leaf),
            intermediates: chain,
        })
    }

    /// First PEM certificate is the leaf; any further certificates are intermediates.
    pub fn from_pem(pem: &[u8], cancel: &CancellationToken) -> Result<Self, MtlsError> {
        cancel.check().map_err(|_| MtlsError::Cancelled)?;
        if pem.len() > MAX_CERT_DER_BYTES.saturating_mul(MAX_INTERMEDIATES.saturating_add(2)) {
            return Err(MtlsError::BoundExceeded {
                limit: MAX_CERT_DER_BYTES.saturating_mul(MAX_INTERMEDIATES.saturating_add(2)),
                requested: pem.len(),
            });
        }
        let mut ders = decode_pem_certs(pem)?;
        if ders.is_empty() {
            return Err(MtlsError::InvalidCertificate);
        }
        let leaf = ders.remove(0);
        Self::from_der(leaf, ders, cancel)
    }

    pub fn leaf_der(&self) -> &[u8] {
        &self.leaf
    }

    pub fn fingerprint(&self) -> CertFingerprint {
        CertFingerprint::sha256(&self.leaf)
    }
}

impl Debug for PresentedCertificate {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PresentedCertificate")
            .field("leaf_len", &self.leaf.len())
            .field("intermediates", &self.intermediates.len())
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
}

impl WorkerClaims {
    pub const fn empty() -> Self {
        Self {
            advertised_id: None,
        }
    }

    pub const fn advertised(id: WorkerId) -> Self {
        Self {
            advertised_id: Some(id),
        }
    }

    pub const fn advertised_id(&self) -> Option<WorkerId> {
        self.advertised_id
    }
}

impl AuthenticatedWorker {
    pub const fn id(&self) -> WorkerId {
        self.id
    }

    pub const fn cert_fingerprint(&self) -> CertFingerprint {
        self.cert_fingerprint
    }

    pub const fn claims(&self) -> WorkerClaims {
        self.claims
    }
}

impl Debug for AuthenticatedWorker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticatedWorker")
            .field("id", &self.id)
            .field("cert_fingerprint", &self.cert_fingerprint)
            .field("claims", &self.claims)
            .finish()
    }
}

impl AuthenticatedController {
    pub const fn id(&self) -> ControllerId {
        self.id
    }

    pub const fn cert_fingerprint(&self) -> CertFingerprint {
        self.cert_fingerprint
    }
}

impl Debug for AuthenticatedController {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticatedController")
            .field("id", &self.id)
            .field("cert_fingerprint", &self.cert_fingerprint)
            .finish()
    }
}

/// Authenticate a worker connection. Identity is the certificate SAN, not the payload.
pub fn authenticate_worker(
    trust: &MtlsTrust,
    presented: &PresentedCertificate,
    payload_worker_id: Option<WorkerId>,
    now: SystemTime,
    cancel: &CancellationToken,
) -> Result<AuthenticatedWorker, MtlsError> {
    let verified = authenticate_peer(
        trust,
        presented,
        PeerRole::Worker,
        now,
        KeyUsage::client_auth(),
        cancel,
    )?;
    let id = parse_worker_id(&verified.identity)?;
    if let Some(claimed) = payload_worker_id
        && claimed != id
    {
        return Err(MtlsError::IdentityMismatch);
    }
    Ok(AuthenticatedWorker {
        id,
        cert_fingerprint: verified.fingerprint,
        claims: WorkerClaims {
            advertised_id: payload_worker_id,
        },
    })
}

/// Authenticate a controller connection. Worker payload cannot supply this identity.
pub fn authenticate_controller(
    trust: &MtlsTrust,
    presented: &PresentedCertificate,
    payload_controller_id: Option<ControllerId>,
    now: SystemTime,
    cancel: &CancellationToken,
) -> Result<AuthenticatedController, MtlsError> {
    let verified = authenticate_peer(
        trust,
        presented,
        PeerRole::Controller,
        now,
        KeyUsage::server_auth(),
        cancel,
    )?;
    let id = parse_controller_id(&verified.identity)?;
    if let Some(claimed) = payload_controller_id
        && claimed != id
    {
        return Err(MtlsError::IdentityMismatch);
    }
    Ok(AuthenticatedController {
        id,
        cert_fingerprint: verified.fingerprint,
    })
}

struct VerifiedPeer {
    identity: String,
    fingerprint: CertFingerprint,
}

fn authenticate_peer(
    trust: &MtlsTrust,
    presented: &PresentedCertificate,
    role: PeerRole,
    now: SystemTime,
    usage: KeyUsage,
    cancel: &CancellationToken,
) -> Result<VerifiedPeer, MtlsError> {
    cancel.check().map_err(|_| MtlsError::Cancelled)?;
    if trust.ca_certs.is_empty() && trust.pins.is_empty() {
        return Err(MtlsError::EmptyTrust);
    }

    let end_entity =
        EndEntityCert::try_from(&presented.leaf).map_err(|_| MtlsError::InvalidCertificate)?;
    let fingerprint = CertFingerprint::sha256(&presented.leaf);

    if !trust.pins.is_empty() && !pin_matches(&trust.pins, &fingerprint) {
        return Err(MtlsError::PinMismatch);
    }

    let unix_now = system_unix(now)?;
    check_validity(&presented.leaf, unix_now)?;

    if !trust.ca_certs.is_empty() {
        let anchors: Vec<TrustAnchor<'_>> = trust
            .ca_certs
            .iter()
            .map(|cert| anchor_from_trusted_cert(cert).map_err(|_| MtlsError::InvalidCertificate))
            .collect::<Result<_, _>>()?;
        let time = UnixTime::since_unix_epoch(Duration::from_secs(unix_now));
        end_entity
            .verify_for_usage(
                webpki::ALL_VERIFICATION_ALGS,
                &anchors,
                &presented.intermediates,
                time,
                usage,
                None,
                None,
            )
            .map_err(map_webpki)?;
    }

    cancel.check().map_err(|_| MtlsError::Cancelled)?;
    let identity = extract_role_uri(&end_entity, role)?;
    Ok(VerifiedPeer {
        identity,
        fingerprint,
    })
}

fn pin_matches(pins: &[CertFingerprint], presented: &CertFingerprint) -> bool {
    let mut matched = false;
    for pin in pins {
        if ct_eq(&pin.0, &presented.0) {
            matched = true;
        }
    }
    matched
}

fn extract_role_uri(cert: &EndEntityCert<'_>, role: PeerRole) -> Result<String, MtlsError> {
    let prefix = match role {
        PeerRole::Worker => WORKER_URI_PREFIX,
        PeerRole::Controller => CONTROLLER_URI_PREFIX,
    };
    let mut found: Option<String> = None;
    for uri in cert.valid_uri_names() {
        if let Some(rest) = uri.strip_prefix(prefix) {
            if rest.is_empty() {
                return Err(MtlsError::IdentityMissing);
            }
            match &found {
                Some(existing) if existing != uri => return Err(MtlsError::InvalidCertificate),
                Some(_) => {}
                None => found = Some(uri.to_owned()),
            }
        }
    }
    found.ok_or(MtlsError::IdentityMissing)
}

fn parse_worker_id(uri: &str) -> Result<WorkerId, MtlsError> {
    let raw = uri
        .strip_prefix(WORKER_URI_PREFIX)
        .ok_or(MtlsError::IdentityMissing)?;
    WorkerId::from_str(raw).map_err(|_| MtlsError::IdentityMissing)
}

fn parse_controller_id(uri: &str) -> Result<ControllerId, MtlsError> {
    let raw = uri
        .strip_prefix(CONTROLLER_URI_PREFIX)
        .ok_or(MtlsError::IdentityMissing)?;
    ControllerId::from_str(raw).map_err(|_| MtlsError::IdentityMissing)
}

fn check_cert_bound(der: &[u8]) -> Result<(), MtlsError> {
    if der.is_empty() {
        return Err(MtlsError::InvalidCertificate);
    }
    if der.len() > MAX_CERT_DER_BYTES {
        return Err(MtlsError::BoundExceeded {
            limit: MAX_CERT_DER_BYTES,
            requested: der.len(),
        });
    }
    Ok(())
}

fn decode_pem_certs(pem: &[u8]) -> Result<Vec<Vec<u8>>, MtlsError> {
    let mut out = Vec::new();
    for item in CertificateDer::pem_slice_iter(pem) {
        let cert = item.map_err(|_| MtlsError::InvalidCertificate)?;
        check_cert_bound(&cert)?;
        out.push(cert.as_ref().to_vec());
        if out.len() > MAX_CA_CERTS {
            return Err(MtlsError::BoundExceeded {
                limit: MAX_CA_CERTS,
                requested: out.len(),
            });
        }
    }
    if out.is_empty() {
        return Err(MtlsError::InvalidCertificate);
    }
    Ok(out)
}

fn system_unix(now: SystemTime) -> Result<u64, MtlsError> {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| MtlsError::InvalidTime)
}

fn check_validity(der: &[u8], now_unix: u64) -> Result<(), MtlsError> {
    let (not_before, not_after) = parse_validity(der)?;
    if now_unix < not_before {
        return Err(MtlsError::NotYetValid);
    }
    if now_unix > not_after {
        return Err(MtlsError::Expired);
    }
    Ok(())
}

fn map_webpki(err: WebPkiError) -> MtlsError {
    match err {
        WebPkiError::CertExpired { .. } => MtlsError::Expired,
        WebPkiError::CertNotValidYet { .. } => MtlsError::NotYetValid,
        WebPkiError::BadDer
        | WebPkiError::BadDerTime
        | WebPkiError::UnsupportedCertVersion
        | WebPkiError::InvalidCertValidity
        | WebPkiError::InvalidSerialNumber => MtlsError::InvalidCertificate,
        _ => MtlsError::Untrusted,
    }
}

fn parse_validity(der: &[u8]) -> Result<(u64, u64), MtlsError> {
    let cert = expect_constructed(der, 0x30)?;
    let (tbs, _) = take_constructed(cert, 0x30)?;
    let mut rest = tbs;
    if rest.first() == Some(&0xa0) {
        let (_, after) = take_tlv(rest)?;
        rest = after;
    }
    let (_, rest) = take_tlv(rest)?;
    let (_, rest) = take_tlv(rest)?;
    let (_, rest) = take_tlv(rest)?;
    let (validity, _) = take_constructed(rest, 0x30)?;
    let (not_before, rest) = take_time(validity)?;
    let (not_after, _) = take_time(rest)?;
    if not_before > not_after {
        return Err(MtlsError::InvalidCertificate);
    }
    Ok((not_before, not_after))
}

fn take_time(input: &[u8]) -> Result<(u64, &[u8]), MtlsError> {
    let (tag, body, rest) = split_tlv(input)?;
    let unix = match tag {
        0x17 => parse_utc_time(body)?,
        0x18 => parse_generalized_time(body)?,
        _ => return Err(MtlsError::InvalidCertificate),
    };
    Ok((unix, rest))
}

fn parse_utc_time(body: &[u8]) -> Result<u64, MtlsError> {
    let raw = std::str::from_utf8(body).map_err(|_| MtlsError::InvalidCertificate)?;
    let digits = raw.strip_suffix('Z').unwrap_or(raw);
    if digits.len() < 10 || !digits.as_bytes().iter().all(|b| b.is_ascii_digit()) {
        return Err(MtlsError::InvalidCertificate);
    }
    let yy = parse_digits(&digits[0..2])?;
    let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
    parse_ymdhms(
        year,
        parse_digits(&digits[2..4])?,
        parse_digits(&digits[4..6])?,
        parse_digits(&digits[6..8])?,
        parse_digits(&digits[8..10])?,
        if digits.len() >= 12 {
            parse_digits(&digits[10..12])?
        } else {
            0
        },
    )
}

fn parse_generalized_time(body: &[u8]) -> Result<u64, MtlsError> {
    let raw = std::str::from_utf8(body).map_err(|_| MtlsError::InvalidCertificate)?;
    let digits = raw.strip_suffix('Z').unwrap_or(raw);
    if digits.len() < 12 || !digits[..12].bytes().all(|b| b.is_ascii_digit()) {
        return Err(MtlsError::InvalidCertificate);
    }
    parse_ymdhms(
        parse_digits(&digits[0..4])?,
        parse_digits(&digits[4..6])?,
        parse_digits(&digits[6..8])?,
        parse_digits(&digits[8..10])?,
        parse_digits(&digits[10..12])?,
        if digits.len() >= 14 && digits[12..14].bytes().all(|b| b.is_ascii_digit()) {
            parse_digits(&digits[12..14])?
        } else {
            0
        },
    )
}

fn parse_digits(raw: &str) -> Result<i32, MtlsError> {
    raw.parse().map_err(|_| MtlsError::InvalidCertificate)
}

fn parse_ymdhms(
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
) -> Result<u64, MtlsError> {
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
        || year < 1970
    {
        return Err(MtlsError::InvalidCertificate);
    }
    let days = days_from_civil(year, month, day)?;
    let secs = i64::from(days) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second);
    u64::try_from(secs).map_err(|_| MtlsError::InvalidCertificate)
}

fn days_from_civil(year: i32, month: i32, day: i32) -> Result<i32, MtlsError> {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(era * 146_097 + doe - 719_468)
}

fn expect_constructed(input: &[u8], tag: u8) -> Result<&[u8], MtlsError> {
    let (body, rest) = take_constructed(input, tag)?;
    if !rest.is_empty() {
        return Err(MtlsError::InvalidCertificate);
    }
    Ok(body)
}

fn take_constructed(input: &[u8], tag: u8) -> Result<(&[u8], &[u8]), MtlsError> {
    let (got, body, rest) = split_tlv(input)?;
    if got != tag {
        return Err(MtlsError::InvalidCertificate);
    }
    Ok((body, rest))
}

fn take_tlv(input: &[u8]) -> Result<(&[u8], &[u8]), MtlsError> {
    let (_, body, rest) = split_tlv(input)?;
    Ok((body, rest))
}

fn split_tlv(input: &[u8]) -> Result<(u8, &[u8], &[u8]), MtlsError> {
    if input.len() < 2 {
        return Err(MtlsError::InvalidCertificate);
    }
    let tag = input[0];
    if tag & 0x1f == 0x1f {
        return Err(MtlsError::InvalidCertificate);
    }
    let (len, header) = parse_der_len(&input[1..])?;
    let start = 1 + header;
    let end = start
        .checked_add(len)
        .ok_or(MtlsError::InvalidCertificate)?;
    if end > input.len() {
        return Err(MtlsError::InvalidCertificate);
    }
    Ok((tag, &input[start..end], &input[end..]))
}

fn parse_der_len(input: &[u8]) -> Result<(usize, usize), MtlsError> {
    let first = *input.first().ok_or(MtlsError::InvalidCertificate)?;
    if first < 0x80 {
        return Ok((first as usize, 1));
    }
    let count = (first & 0x7f) as usize;
    if count == 0 || count > 3 || input.len() < 1 + count {
        return Err(MtlsError::InvalidCertificate);
    }
    let mut len = 0usize;
    for b in &input[1..1 + count] {
        len = len
            .checked_mul(256)
            .and_then(|n| n.checked_add(*b as usize))
            .ok_or(MtlsError::InvalidCertificate)?;
    }
    if len < 0x80 {
        return Err(MtlsError::InvalidCertificate);
    }
    Ok((len, 1 + count))
}

fn ct_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut acc = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        acc |= a ^ b;
    }
    acc == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_TABLE[(byte >> 4) as usize] as char);
        out.push(HEX_TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
        IsCa, KeyPair, KeyUsagePurpose, SanType, date_time_ymd,
    };

    const WORKER_A: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const WORKER_B: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789cd";
    const CONTROLLER_A: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ef";
    // 2023-11-14 22:13:20 UTC — inside the default fixture window.
    const NOW_UNIX: u64 = 1_700_000_000;

    struct Issued {
        ca_der: Vec<u8>,
        ca_pem: String,
        leaf_der: Vec<u8>,
        leaf_pem: String,
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(NOW_UNIX)
    }

    fn worker_id(raw: &str) -> WorkerId {
        WorkerId::from_str(raw).expect("worker id")
    }

    fn controller_id(raw: &str) -> ControllerId {
        ControllerId::from_str(raw).expect("controller id")
    }

    fn issue(
        uri: &str,
        eku: ExtendedKeyUsagePurpose,
        not_before: (i32, u8, u8),
        not_after: (i32, u8, u8),
        foreign_ca: bool,
    ) -> Issued {
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params.distinguished_name.push(
            DnType::CommonName,
            if foreign_ca {
                "RapidLM Foreign Test CA"
            } else {
                "RapidLM Test CA"
            },
        );
        ca_params.not_before = date_time_ymd(not_before.0 - 1, not_before.1, not_before.2);
        ca_params.not_after = date_time_ymd(not_after.0 + 1, not_after.1, not_after.2);
        let ca_key = KeyPair::generate().expect("ca key");
        let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");

        let mut leaf_params = CertificateParams::new(Vec::<String>::new()).expect("leaf params");
        leaf_params.subject_alt_names = vec![SanType::URI(uri.try_into().expect("uri san"))];
        leaf_params.extended_key_usages = vec![eku];
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.distinguished_name = DistinguishedName::new();
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, "rapidlm-peer");
        leaf_params.not_before = date_time_ymd(not_before.0, not_before.1, not_before.2);
        leaf_params.not_after = date_time_ymd(not_after.0, not_after.1, not_after.2);
        let leaf_key = KeyPair::generate().expect("leaf key");
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &ca_cert, &ca_key)
            .expect("leaf cert");

        Issued {
            ca_der: ca_cert.der().to_vec(),
            ca_pem: ca_cert.pem(),
            leaf_der: leaf_cert.der().to_vec(),
            leaf_pem: leaf_cert.pem(),
        }
    }

    fn valid_window() -> ((i32, u8, u8), (i32, u8, u8)) {
        ((2023, 11, 1), (2023, 12, 1))
    }

    fn worker_cert() -> Issued {
        let (start, end) = valid_window();
        issue(
            &format!("{WORKER_URI_PREFIX}{WORKER_A}"),
            ExtendedKeyUsagePurpose::ClientAuth,
            start,
            end,
            false,
        )
    }

    fn trust_for(issued: &Issued) -> MtlsTrust {
        MtlsTrust::new([issued.ca_der.clone()], [], &live()).expect("trust")
    }

    fn presented(issued: &Issued) -> PresentedCertificate {
        PresentedCertificate::from_der(issued.leaf_der.clone(), [], &live()).expect("presented")
    }

    #[test]
    fn ca_signed_worker_binds_certificate_identity() {
        let issued = worker_cert();
        let authn = authenticate_worker(
            &trust_for(&issued),
            &presented(&issued),
            Some(worker_id(WORKER_A)),
            now(),
            &live(),
        )
        .expect("authn");
        assert_eq!(authn.id(), worker_id(WORKER_A));
        assert_eq!(
            authn.cert_fingerprint(),
            CertFingerprint::sha256(&issued.leaf_der)
        );
        assert_eq!(authn.claims().advertised_id(), Some(worker_id(WORKER_A)));
    }

    #[test]
    fn expired_cert_rejected() {
        let issued = issue(
            &format!("{WORKER_URI_PREFIX}{WORKER_A}"),
            ExtendedKeyUsagePurpose::ClientAuth,
            (2023, 1, 1),
            (2023, 11, 1),
            false,
        );
        let err = authenticate_worker(
            &trust_for(&issued),
            &presented(&issued),
            Some(worker_id(WORKER_A)),
            now(),
            &live(),
        )
        .expect_err("expired");
        assert_eq!(err, MtlsError::Expired);
    }

    #[test]
    fn untrusted_foreign_ca_rejected() {
        let trusted = worker_cert();
        let (start, end) = valid_window();
        let foreign = issue(
            &format!("{WORKER_URI_PREFIX}{WORKER_A}"),
            ExtendedKeyUsagePurpose::ClientAuth,
            start,
            end,
            true,
        );
        let err = authenticate_worker(
            &trust_for(&trusted),
            &presented(&foreign),
            Some(worker_id(WORKER_A)),
            now(),
            &live(),
        )
        .expect_err("untrusted");
        assert_eq!(err, MtlsError::Untrusted);
    }

    #[test]
    fn payload_cannot_override_certificate_identity() {
        let issued = worker_cert();
        let err = authenticate_worker(
            &trust_for(&issued),
            &presented(&issued),
            Some(worker_id(WORKER_B)),
            now(),
            &live(),
        )
        .expect_err("mismatch");
        assert_eq!(err, MtlsError::IdentityMismatch);
    }

    #[test]
    fn pin_mismatch_rejected_even_when_ca_valid() {
        let issued = worker_cert();
        let (start, end) = valid_window();
        let other = issue(
            &format!("{WORKER_URI_PREFIX}{WORKER_B}"),
            ExtendedKeyUsagePurpose::ClientAuth,
            start,
            end,
            false,
        );
        let pin = CertFingerprint::sha256(&other.leaf_der);
        let trust = MtlsTrust::new([issued.ca_der.clone()], [pin], &live()).expect("trust");
        let err = authenticate_worker(&trust, &presented(&issued), None, now(), &live())
            .expect_err("pin");
        assert_eq!(err, MtlsError::PinMismatch);
    }

    #[test]
    fn pinned_identity_accepted_without_payload() {
        let issued = worker_cert();
        let pin = CertFingerprint::sha256(&issued.leaf_der);
        let trust = MtlsTrust::new([issued.ca_der.clone()], [pin], &live()).expect("trust");
        let authn =
            authenticate_worker(&trust, &presented(&issued), None, now(), &live()).expect("pinned");
        assert_eq!(authn.id(), worker_id(WORKER_A));
        assert!(authn.claims().advertised_id().is_none());
    }

    #[test]
    fn pin_only_trust_rejects_different_leaf_same_san() {
        let first = worker_cert();
        let (start, end) = valid_window();
        let second = issue(
            &format!("{WORKER_URI_PREFIX}{WORKER_A}"),
            ExtendedKeyUsagePurpose::ClientAuth,
            start,
            end,
            false,
        );
        let pin = CertFingerprint::sha256(&first.leaf_der);
        let trust = MtlsTrust::new([], [pin], &live()).expect("pin only");
        authenticate_worker(&trust, &presented(&first), None, now(), &live()).expect("first pin");
        let err = authenticate_worker(&trust, &presented(&second), None, now(), &live())
            .expect_err("second");
        assert_eq!(err, MtlsError::PinMismatch);
    }

    #[test]
    fn empty_trust_rejected() {
        let err = MtlsTrust::new([], [], &live()).expect_err("empty");
        assert_eq!(err, MtlsError::EmptyTrust);
    }

    #[test]
    fn cancelled_before_auth() {
        let issued = worker_cert();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = authenticate_worker(
            &trust_for(&issued),
            &presented(&issued),
            None,
            now(),
            &cancel,
        )
        .expect_err("cancel");
        assert_eq!(err, MtlsError::Cancelled);
    }

    #[test]
    fn controller_identity_is_mutual() {
        let (start, end) = valid_window();
        let issued = issue(
            &format!("{CONTROLLER_URI_PREFIX}{CONTROLLER_A}"),
            ExtendedKeyUsagePurpose::ServerAuth,
            start,
            end,
            false,
        );
        let authn = authenticate_controller(
            &trust_for(&issued),
            &presented(&issued),
            Some(controller_id(CONTROLLER_A)),
            now(),
            &live(),
        )
        .expect("controller");
        assert_eq!(authn.id(), controller_id(CONTROLLER_A));
    }

    #[test]
    fn worker_uri_required_on_worker_cert() {
        let (start, end) = valid_window();
        let issued = issue(
            &format!("{CONTROLLER_URI_PREFIX}{CONTROLLER_A}"),
            ExtendedKeyUsagePurpose::ClientAuth,
            start,
            end,
            false,
        );
        let err = authenticate_worker(
            &trust_for(&issued),
            &presented(&issued),
            None,
            now(),
            &live(),
        )
        .expect_err("role");
        assert_eq!(err, MtlsError::IdentityMissing);
    }

    #[test]
    fn debug_omits_certificate_bytes() {
        let issued = worker_cert();
        let presented = presented(&issued);
        let trust = trust_for(&issued);
        let authn = authenticate_worker(&trust, &presented, None, now(), &live()).expect("authn");
        let leaf_hex = hex_encode(&issued.leaf_der);
        for rendered in [
            format!("{presented:?}"),
            format!("{trust:?}"),
            format!("{authn:?}"),
            format!("{}", MtlsError::Untrusted),
        ] {
            assert!(!rendered.contains(&leaf_hex), "leaked leaf DER: {rendered}");
            assert!(
                !rendered.contains("BEGIN CERTIFICATE"),
                "leaked PEM: {rendered}"
            );
        }
        assert!(!issued.leaf_pem.is_empty());
    }

    #[test]
    fn pem_round_trip_matches_der() {
        let issued = worker_cert();
        let from_pem =
            PresentedCertificate::from_pem(issued.leaf_pem.as_bytes(), &live()).expect("pem");
        assert_eq!(from_pem.leaf_der(), issued.leaf_der);
        let trust = MtlsTrust::from_ca_pem(issued.ca_pem.as_bytes(), [], &live()).expect("ca pem");
        authenticate_worker(&trust, &from_pem, None, now(), &live()).expect("pem authn");
    }

    #[test]
    fn not_yet_valid_rejected() {
        let issued = issue(
            &format!("{WORKER_URI_PREFIX}{WORKER_A}"),
            ExtendedKeyUsagePurpose::ClientAuth,
            (2023, 12, 1),
            (2024, 1, 1),
            false,
        );
        let err = authenticate_worker(
            &trust_for(&issued),
            &presented(&issued),
            None,
            now(),
            &live(),
        )
        .expect_err("future");
        assert_eq!(err, MtlsError::NotYetValid);
    }
}
