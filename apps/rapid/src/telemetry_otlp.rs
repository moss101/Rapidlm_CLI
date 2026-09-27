//! `[telemetry.otlp]` (SEAM-07 AC-04): an opt-in exporter that sends
//! redacted turn records to an OpenTelemetry collector.
//!
//! Off unless the user's own config names an endpoint — a project cannot
//! redirect telemetry — and with it off, nothing is built: no thread, no
//! socket. With it on:
//!
//! - every record passes `crates/telemetry`'s redaction first (forbidden
//!   keys refused, registered secrets masked) — only what that pipeline lets
//!   out reaches the transport;
//! - the transport hands a record to one worker thread through a bounded
//!   queue: a full queue drops the record, never blocks the turn;
//! - the worker dials only through an egress gate that allows exactly the
//!   collector, and every decision it makes is kept as a receipt — in
//!   memory, and appended to `<RapidLM home>/telemetry/egress-receipts.jsonl`;
//! - a dead or slow collector loses records (counted), and is never a
//!   caller-visible failure.
//!
//! The ledger has no egress event kind; adding one changes the SDK wire
//! catalog, so the receipt log is a file beside the ledger instead.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::provider_egress::{EgressReceipt, ProviderEgress};

/// Records waiting for the worker, at most.
pub const MAX_PENDING_RECORDS: usize = 64;
/// One export's timeout.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);
/// Receipts kept in memory, at most (the log file keeps them all).
const MAX_KEPT_RECEIPTS: usize = 256;
/// The receipt log is started over past this size.
const MAX_RECEIPT_LOG_BYTES: u64 = 1024 * 1024;

/// Where records go: the collector's URL, split.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Collector {
    pub url: String,
    pub https: bool,
    pub host: String,
    pub port: u16,
}

impl Collector {
    /// An endpoint `crates/telemetry` accepts (https, or http on loopback;
    /// no userinfo, query or fragment). A bare origin posts to `/v1/logs`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        telemetry::CollectorEndpoint::parse(raw).map_err(|_| {
            format!("telemetry.otlp.endpoint '{raw}' is not an https URL (or http on loopback)")
        })?;
        let (scheme, rest) = raw.split_once("://").unwrap_or(("https", raw));
        let https = scheme == "https";
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| format!("telemetry.otlp.endpoint '{raw}' has a bad port"))?,
            ),
            None => (authority.to_owned(), if https { 443 } else { 80 }),
        };
        let path = if path.is_empty() || path == "/" {
            "/v1/logs"
        } else {
            path
        };
        Ok(Self {
            url: format!("{scheme}://{host}:{port}{path}"),
            https,
            host,
            port,
        })
    }
}

/// `[telemetry.otlp] endpoint` in the user's config, if set. A project's
/// config is not read: where telemetry goes is the user's decision.
pub fn load(user_config: Option<&Path>) -> Result<Option<Collector>, String> {
    let Some(path) = user_config else {
        return Ok(None);
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let root: toml::Value =
        toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    let Some(otlp) = root
        .get("telemetry")
        .and_then(|telemetry| telemetry.get("otlp"))
    else {
        return Ok(None);
    };
    let table = otlp
        .as_table()
        .ok_or_else(|| "telemetry.otlp must be a table".to_owned())?;
    if let Some(key) = table.keys().find(|key| key.as_str() != "endpoint") {
        return Err(format!("telemetry.otlp.{key} is not a telemetry key"));
    }
    match table.get("endpoint") {
        None => Ok(None),
        Some(value) => {
            let raw = value
                .as_str()
                .ok_or_else(|| "telemetry.otlp.endpoint must be a string".to_owned())?;
            Collector::parse(raw).map(Some)
        }
    }
}

/// The transport: a bounded hand-off to one worker that posts each record
/// through the egress gate.
pub struct HttpOtlpTransport {
    sender: Mutex<Option<SyncSender<Vec<u8>>>>,
    receipts: Arc<Mutex<Vec<EgressReceipt>>>,
    /// Records the worker could not deliver.
    failed: Arc<std::sync::atomic::AtomicU64>,
    /// Records the worker delivered.
    delivered: Arc<std::sync::atomic::AtomicU64>,
    /// Records handed to the worker.
    sent: std::sync::atomic::AtomicU64,
}

impl HttpOtlpTransport {
    /// Start the worker for `collector`, appending receipts to `receipt_log`
    /// when one is given.
    pub fn start(collector: Collector, receipt_log: Option<PathBuf>) -> Arc<Self> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(MAX_PENDING_RECORDS);
        let transport = Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            receipts: Arc::new(Mutex::new(Vec::new())),
            failed: Arc::default(),
            delivered: Arc::default(),
            sent: std::sync::atomic::AtomicU64::new(0),
        });
        let (receipts, failed, delivered) = (
            Arc::clone(&transport.receipts),
            Arc::clone(&transport.failed),
            Arc::clone(&transport.delivered),
        );
        std::thread::spawn(move || {
            worker(
                collector,
                receiver,
                receipts,
                failed,
                delivered,
                receipt_log,
            )
        });
        transport
    }

    /// Every egress decision so far, oldest first (the last
    /// `MAX_KEPT_RECEIPTS`).
    pub fn receipts(&self) -> Vec<EgressReceipt> {
        self.receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn delivered(&self) -> u64 {
        self.delivered.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn failed(&self) -> u64 {
        self.failed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait, at most `limit`, for every record handed over to be delivered
    /// or given up on — for a process about to exit. Never an error: what
    /// is still pending when the limit passes is lost.
    pub fn flush(&self, limit: Duration) {
        let deadline = std::time::Instant::now() + limit;
        while self.delivered() + self.failed() < self.sent.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl telemetry::OtlpTransport for HttpOtlpTransport {
    fn export(
        &self,
        payload: &[u8],
        cancel: &telemetry::CancellationToken,
    ) -> Result<(), telemetry::TelemetryError> {
        if cancel.is_cancelled() {
            return Err(telemetry::TelemetryError::Cancelled);
        }
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(sender) = sender.as_ref() else {
            return Err(telemetry::TelemetryError::Closed);
        };
        // Never blocks: a full queue (a slow or dead collector) drops.
        match sender.try_send(payload.to_vec()) {
            Ok(()) => {
                self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(telemetry::TelemetryError::BoundExceeded),
            Err(TrySendError::Disconnected(_)) => Err(telemetry::TelemetryError::Closed),
        }
    }

    fn network_enabled(&self) -> bool {
        true
    }
}

fn worker(
    collector: Collector,
    receiver: Receiver<Vec<u8>>,
    receipts: Arc<Mutex<Vec<EgressReceipt>>>,
    failed: Arc<std::sync::atomic::AtomicU64>,
    delivered: Arc<std::sync::atomic::AtomicU64>,
    receipt_log: Option<PathBuf>,
) {
    use llm_router::providers::openai_compatible::{Http1Transport, StaticWireAuth};
    for payload in receiver {
        let outcome = (|| -> Result<u16, String> {
            let gate = Arc::new(
                ProviderEgress::for_endpoint(
                    collector.https,
                    &collector.host,
                    collector.port,
                    None,
                )
                .map_err(|err| format!("egress: {err}"))?,
            );
            // The collector takes no credential: the placeholder bearer is
            // what the raw writer requires, and says nothing secret.
            let auth = StaticWireAuth::bearer("rapidlm-telemetry".to_owned())
                .map_err(|err| format!("{err}"))?;
            let transport =
                Http1Transport::with_limits(auth, EXPORT_TIMEOUT, 64 * 1024).with_dial_gate(
                    Arc::clone(&gate) as Arc<dyn llm_router::providers::dial::DialGate>,
                );
            let result = transport.post_raw(
                &collector.url,
                &[("Content-Type".to_owned(), "application/json".to_owned())],
                &payload,
                "rapidlm-telemetry",
                &llm_router::provider::CancellationToken::new(),
            );
            record_receipts(&receipts, receipt_log.as_deref(), gate.receipts());
            let response = result.map_err(|err| format!("{err}"))?;
            Ok(response.status)
        })();
        match outcome {
            Ok(status) if (200..300).contains(&status) => {
                delivered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            _ => {
                failed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

fn record_receipts(
    kept: &Mutex<Vec<EgressReceipt>>,
    log: Option<&Path>,
    receipts: Vec<EgressReceipt>,
) {
    if receipts.is_empty() {
        return;
    }
    if let Some(log) = log {
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::metadata(log).is_ok_and(|meta| meta.len() > MAX_RECEIPT_LOG_BYTES) {
            let _ = std::fs::remove_file(log);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            for receipt in &receipts {
                let line = serde_json::json!({
                    "at": protocol::TraceId::new().to_string(),
                    "client": "telemetry",
                    "allowed": receipt.allowed,
                    "dialled": receipt.dialled,
                    "reason": receipt.reason,
                });
                let _ = writeln!(file, "{line}");
            }
        }
    }
    let mut kept = kept
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    kept.extend(receipts);
    let excess = kept.len().saturating_sub(MAX_KEPT_RECEIPTS);
    kept.drain(..excess);
}

/// What a finished turn is exported as. Counts and classes only: no prompt,
/// answer, path or command text is ever a field.
#[derive(Clone, Debug, Default)]
pub struct TurnRecord<'a> {
    pub status: &'a str,
    pub tokens: u64,
    pub tool_calls: u64,
    /// Provider-reported cost; `None` is unknown and exported as such.
    pub cost_usd_micros: Option<u64>,
}

/// An exporter: `crates/telemetry` (redaction, the sink) over the HTTP
/// transport.
pub struct Exporter {
    telemetry: telemetry::Telemetry,
    transport: Arc<HttpOtlpTransport>,
}

impl Exporter {
    pub fn start(collector: Collector, receipt_log: Option<PathBuf>) -> Result<Self, String> {
        let endpoint = telemetry::CollectorEndpoint::parse(&collector.url)
            .map_err(|_| "the collector endpoint was refused".to_owned())?;
        let policy = telemetry::ExporterPolicy::locked(Some(endpoint.clone()));
        let transport = HttpOtlpTransport::start(collector, receipt_log);
        let sink = telemetry::OtlpSink::with_collector(
            endpoint,
            Arc::clone(&transport) as Arc<dyn telemetry::OtlpTransport>,
            &policy,
        )
        .map_err(|err| format!("{err}"))?;
        let telemetry = telemetry::Telemetry::builder(protocol::TelemetryConfig::default(), policy)
            .otlp_sink(sink)
            .map_err(|err| format!("{err}"))?
            .build();
        Ok(Self {
            telemetry,
            transport,
        })
    }

    /// Mask `secret` in every record from now on.
    pub fn register_secret(&self, secret: &str) {
        let _ = self
            .telemetry
            .register_canary(secret.as_bytes(), &telemetry::CancellationToken::new());
    }

    /// Export one finished turn. Loss is never the caller's failure.
    pub fn record_turn(&self, turn: &TurnRecord<'_>) {
        let (tokens, tool_calls) = (turn.tokens.to_string(), turn.tool_calls.to_string());
        let cost = turn
            .cost_usd_micros
            .map_or_else(|| "unknown".to_owned(), |micros| micros.to_string());
        let _ = self.telemetry.emit_log(
            telemetry::LogLevel::Info,
            "rapid.turn",
            &protocol::TraceContext::root(),
            turn.status,
            &[
                ("tokens", tokens.as_str()),
                ("tool_calls", tool_calls.as_str()),
                ("cost_usd_micros", cost.as_str()),
            ],
            &telemetry::CancellationToken::new(),
        );
    }

    pub fn transport(&self) -> &HttpOtlpTransport {
        &self.transport
    }
}

/// The process's exporter: built once from the user's config, `None` when
/// telemetry is off (the default) — then nothing is started at all.
pub fn exporter(user_home: Option<&Path>) -> Option<&'static Exporter> {
    static EXPORTER: std::sync::OnceLock<Option<Exporter>> = std::sync::OnceLock::new();
    EXPORTER
        .get_or_init(|| {
            let home = user_home?;
            let collector = match load(Some(&home.join("config.toml"))) {
                Ok(collector) => collector?,
                Err(reason) => {
                    eprintln!("warning: telemetry is off: {reason}");
                    return None;
                }
            };
            match Exporter::start(
                collector,
                Some(home.join("telemetry").join("egress-receipts.jsonl")),
            ) {
                Ok(exporter) => Some(exporter),
                Err(reason) => {
                    eprintln!("warning: telemetry is off: {reason}");
                    None
                }
            }
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// A collector on loopback that captures each request it answers.
    fn collector(answer: &'static str) -> (Collector, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // Head, then the body its Content-Length promises.
                let mut want: Option<usize> = None;
                loop {
                    let read = stream.read(&mut buf).unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..read]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    if want.is_none()
                        && let Some(head_end) = text.find("\r\n\r\n")
                    {
                        let length = text[..head_end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        want = Some(head_end + 4 + length);
                    }
                    if want.is_some_and(|want| raw.len() >= want) {
                        break;
                    }
                }
                captured
                    .lock()
                    .expect("captured")
                    .push(String::from_utf8_lossy(&raw).into_owned());
                let _ = std::io::Write::write_all(&mut stream, answer.as_bytes());
            }
        });
        (
            Collector::parse(&format!("http://127.0.0.1:{port}")).expect("collector"),
            seen,
        )
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";

    fn wait(until: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !until() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn off_unless_the_users_config_names_an_endpoint() {
        let dir =
            std::env::temp_dir().join(format!("rapidlm-otlp-load-{}", protocol::TraceId::new()));
        std::fs::create_dir_all(&dir).expect("dir");
        let config = dir.join("config.toml");
        assert_eq!(load(Some(&config)), Ok(None), "no file: off");
        std::fs::write(&config, "[models]\ndefault = \"x\"\n").expect("write");
        assert_eq!(load(Some(&config)), Ok(None), "no table: off");
        std::fs::write(
            &config,
            "[telemetry.otlp]\nendpoint = \"https://otel.example:4318\"\n",
        )
        .expect("write");
        let collector = load(Some(&config)).expect("load").expect("on");
        assert_eq!(collector.url, "https://otel.example:4318/v1/logs");
        for bad in [
            "[telemetry.otlp]\nendpoint = \"http://otel.example:4318\"\n",
            "[telemetry.otlp]\nendpoint = \"https://user@otel.example\"\n",
            "[telemetry.otlp]\nendpoint = 7\n",
            "[telemetry.otlp]\nsample = 1\n",
        ] {
            std::fs::write(&config, bad).expect("write");
            assert!(load(Some(&config)).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_redacted_records_leave_and_every_dial_is_a_receipt() {
        let (collector, seen) = collector(OK);
        let dir = std::env::temp_dir().join(format!("rapidlm-otlp-{}", protocol::TraceId::new()));
        let log = dir.join("telemetry").join("egress-receipts.jsonl");
        let exporter = Exporter::start(collector.clone(), Some(log.clone())).expect("exporter");
        exporter.register_secret("sk-live-CANARY-0123456789");
        exporter.record_turn(&TurnRecord {
            status: "completed sk-live-CANARY-0123456789",
            tokens: 42,
            tool_calls: 3,
            cost_usd_micros: None,
        });
        wait(|| exporter.transport().delivered() == 1);
        let requests = seen.lock().expect("seen").clone();
        assert_eq!(requests.len(), 1);
        let body = &requests[0];
        assert!(body.starts_with("POST /v1/logs HTTP/1.1"), "{body}");
        assert!(body.contains("\"tokens\""), "{body}");
        assert!(
            body.contains("unknown"),
            "an unknown cost left as unknown: {body}"
        );
        assert!(
            !body.contains("sk-live-CANARY-0123456789"),
            "a secret left: {body}"
        );
        let receipts = exporter.transport().receipts();
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert!(receipts[0].allowed);
        assert_eq!(
            receipts[0].dialled,
            format!("http://127.0.0.1:{}", collector.port)
        );
        let logged = std::fs::read_to_string(&log).expect("receipt log");
        assert!(logged.contains("\"client\":\"telemetry\""), "{logged}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_collector_never_fails_or_holds_up_the_caller() {
        // A collector that accepts and never answers: every export waits
        // out its timeout on the worker, the queue fills, and the caller
        // still never waits on any of it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
        let collector = Collector::parse(&format!("http://127.0.0.1:{port}")).expect("collector");
        let exporter = Exporter::start(collector, None).expect("exporter");
        let started = std::time::Instant::now();
        for _ in 0..(MAX_PENDING_RECORDS * 3) {
            exporter.record_turn(&TurnRecord {
                status: "completed",
                ..TurnRecord::default()
            });
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the caller waited on a dead collector: {:?}",
            started.elapsed()
        );
        wait(|| exporter.transport().failed() >= 1);
        assert_eq!(exporter.transport().delivered(), 0);
    }
}
