//! The passive update notice (SEAM-14-1): one line when the terminal UI
//! exits, saying a newer release exists.
//!
//! Off unless the user turns it on and names a server — `[update] check =
//! true` and `[update] url = "https://…/manifest.json"` in the user's config
//! (decision D-3: there is no built-in update server; a destination the user
//! did not choose is one this binary does not dial). Even then it never runs
//! in CI, without a terminal, or when `RAPIDLM_NO_UPDATE_CHECK` is set; it
//! asks at most once in 24 hours, with a 3-second limit; it goes out through
//! the egress gate like every other outbound call this binary makes, and
//! every decision leaves a receipt in `<home>/update/egress-receipts.jsonl`.
//!
//! The check runs on a thread of its own, so nothing waits on it; what it
//! learns is kept in `<home>/update/state.json` and shown at the next exit —
//! once per new version. The notice never installs anything: `rapid update`
//! does that, and only when asked.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::provider_egress::ProviderEgress;

/// The most often a check is made.
pub(crate) const CHECK_EVERY_MS: i64 = 24 * 60 * 60 * 1000;
/// One check's limit, connect to last byte.
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
/// The largest manifest read.
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// The receipt log is started over past this size.
const MAX_RECEIPT_LOG_BYTES: u64 = 256 * 1024;

/// The manifest's address, split for the egress gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestUrl {
    pub url: String,
    pub https: bool,
    pub host: String,
    pub port: u16,
}

impl ManifestUrl {
    /// An address the HTTP client can use: https (or http on loopback, for a
    /// server the user runs), no user name, query or fragment, and a path —
    /// a manifest is a file, not an origin.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        telemetry::CollectorEndpoint::parse(raw)
            .map_err(|_| format!("update.url '{raw}' is not an https URL (or http on loopback)"))?;
        let (scheme, rest) = raw.split_once("://").unwrap_or(("https", raw));
        let https = scheme == "https";
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        if path.is_empty() || path == "/" {
            return Err(format!(
                "update.url '{raw}' names no manifest file (it needs a path, such as /rapid/latest.json)"
            ));
        }
        if authority.starts_with('[') {
            return Err(format!(
                "update.url '{raw}': an IPv6 address is not supported; name the server by a host name or an IPv4 address"
            ));
        }
        let default_port = if https { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| format!("update.url '{raw}' has a bad port"))?,
            ),
            None => (authority.to_owned(), default_port),
        };
        Ok(Self {
            url: format!("{scheme}://{host}:{port}{path}"),
            https,
            host,
            port,
        })
    }
}

/// `[update]` in the user's config.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UpdateConfig {
    /// `check = true`: the passive notice is on.
    pub check: bool,
    pub url: Option<ManifestUrl>,
}

/// The `[update]` table of the user's config file: unknown keys refused,
/// `check = true` refused without a `url`. A project's config is not read:
/// where the binary phones is the user's decision.
pub(crate) fn load_config(
    user_config: Option<&Path>,
    env_url: Option<&str>,
) -> Result<UpdateConfig, String> {
    let Some(path) = user_config else {
        return Ok(UpdateConfig::default());
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(UpdateConfig::default());
    };
    let root: toml::Value =
        toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    let Some(section) = root.get("update") else {
        return Ok(UpdateConfig::default());
    };
    let table = section
        .as_table()
        .ok_or_else(|| "update must be a table".to_owned())?;
    if let Some(key) = table
        .keys()
        .find(|key| !matches!(key.as_str(), "check" | "url"))
    {
        return Err(format!("update.{key} is not an update key"));
    }
    let check = match table.get("check") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "update.check must be true or false".to_owned())?,
    };
    let url = match table.get("url") {
        None => None,
        Some(value) => Some(ManifestUrl::parse(
            value
                .as_str()
                .ok_or_else(|| "update.url must be a string".to_owned())?,
        )?),
    };
    // The URL `rapid update` already takes from the environment is a
    // destination the user chose too; `update.url` wins when both are set.
    let url = match (url, env_url.filter(|raw| !raw.trim().is_empty())) {
        (Some(url), _) => Some(url),
        (None, Some(raw)) if check => Some(
            ManifestUrl::parse(raw.trim())
                .map_err(|err| err.replace("update.url", "RAPIDLM_UPDATE_URL"))?,
        ),
        (None, _) => None,
    };
    if check && url.is_none() {
        return Err(
            "update.check needs update.url (or RAPIDLM_UPDATE_URL): there is no built-in update server to ask"
                .to_owned(),
        );
    }
    Ok(UpdateConfig { check, url })
}

/// What the check remembers between runs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct State {
    /// When a check was last made (a failed one counts: no hammering).
    pub checked_at_ms: Option<i64>,
    /// The newer version the last successful check found.
    pub latest: Option<String>,
    /// The version the user was last told about.
    pub announced: Option<String>,
}

impl State {
    /// The state file, tolerantly: anything unreadable is a fresh start.
    pub(crate) fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Self::default();
        };
        let text_of = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|text| text.len() <= 64)
                .map(str::to_owned)
        };
        Self {
            checked_at_ms: value
                .get("checked_at_ms")
                .and_then(serde_json::Value::as_i64),
            latest: text_of("latest"),
            announced: text_of("announced"),
        }
    }

    /// Written whole and renamed into place, private to the user. Best
    /// effort: a state that cannot be kept means the next run checks again.
    pub(crate) fn save(&self, path: &Path) {
        use std::io::Write as _;
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let body = serde_json::json!({
            "checked_at_ms": self.checked_at_ms,
            "latest": self.latest,
            "announced": self.announced,
        })
        .to_string();
        let written = options
            .open(&temp)
            .and_then(|mut file| file.write_all(body.as_bytes()));
        if written.is_err() || std::fs::rename(&temp, path).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

/// Why a check is not made.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Skip {
    /// Asked for `check = false`, or not at all.
    Off,
    /// Running in continuous integration.
    Ci,
    /// No terminal to tell: headless and piped runs print nothing extra.
    NoTerminal,
    /// `RAPIDLM_NO_UPDATE_CHECK` is set.
    OptedOut,
    /// One was made within the last 24 hours.
    Throttled,
}

fn truthy(value: Option<String>) -> bool {
    value.is_some_and(|value| {
        let value = value.trim().to_ascii_lowercase();
        !value.is_empty() && value != "0" && value != "false"
    })
}

/// Whether the check is skipped, and why. Pure: the environment, the
/// terminal, the state and the clock are all arguments.
pub(crate) fn skip_reason(
    config: &UpdateConfig,
    env: &dyn Fn(&str) -> Option<String>,
    terminal: bool,
    state: &State,
    now_ms: i64,
) -> Option<Skip> {
    if !config.check {
        return Some(Skip::Off);
    }
    if truthy(env("RAPIDLM_NO_UPDATE_CHECK")) {
        return Some(Skip::OptedOut);
    }
    const CI_VARIABLES: [&str; 8] = [
        "CI",
        "CONTINUOUS_INTEGRATION",
        "BUILD_NUMBER",
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "BUILDKITE",
        "TF_BUILD",
        "JENKINS_URL",
    ];
    if CI_VARIABLES.iter().any(|name| truthy(env(name))) {
        return Some(Skip::Ci);
    }
    if !terminal {
        return Some(Skip::NoTerminal);
    }
    // A clock set back does not stop checks for a day: only a check in the
    // last 24 hours, not one in the future, throttles.
    if state
        .checked_at_ms
        .is_some_and(|at| (0..CHECK_EVERY_MS).contains(&(now_ms - at)))
    {
        return Some(Skip::Throttled);
    }
    None
}

/// Ask the manifest server, through the egress gate, whether a version newer
/// than `current` exists: `Some(version)` if so. Every dial decision leaves
/// a receipt in `receipts_log`, whatever the outcome.
pub(crate) fn check(
    url: &ManifestUrl,
    current: &str,
    timeout: Duration,
    receipts_log: &Path,
) -> Result<Option<String>, String> {
    use llm_router::providers::openai_compatible::{Http1Transport, StaticWireAuth};
    let gate = Arc::new(
        ProviderEgress::for_client(
            security::NetworkClient::Tool,
            url.https,
            &url.host,
            url.port,
            None,
        )
        .map_err(|err| format!("egress: {err}"))?,
    );
    // The manifest takes no credential and a GET sends none; the transport
    // wants an auth value to be built at all.
    let auth =
        StaticWireAuth::bearer("rapidlm-update".to_owned()).map_err(|err| err.to_string())?;
    let transport = Http1Transport::with_limits(auth, timeout, MAX_MANIFEST_BYTES)
        .with_dial_gate(Arc::clone(&gate) as Arc<dyn llm_router::providers::dial::DialGate>);
    let result = transport.get_raw(
        &url.url,
        &[("Accept".to_owned(), "application/json".to_owned())],
        &llm_router::provider::CancellationToken::new(),
    );
    record_receipts(receipts_log, &gate.receipts());
    let response =
        result.map_err(|err| format!("the update server could not be reached: {err}"))?;
    if !(200..300).contains(&response.status) {
        return Err(format!("the update server answered {}", response.status));
    }
    let text = String::from_utf8_lossy(&response.body);
    let manifest = crate::update_serve::parse_manifest(&text)?;
    match crate::update_serve::ensure_newer(current, &manifest.version) {
        Ok(()) => Ok(Some(manifest.version)),
        // Not newer: nothing to say. A version that is not one is an error.
        Err(reason) if reason.starts_with("refusing to install") => Ok(None),
        Err(reason) => Err(reason),
    }
}

/// Append the gate's decisions to the receipt log, started over past its
/// bound. Best effort.
fn record_receipts(log: &Path, receipts: &[crate::provider_egress::EgressReceipt]) {
    use std::io::Write as _;
    if receipts.is_empty() {
        return;
    }
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(log).is_ok_and(|meta| meta.len() > MAX_RECEIPT_LOG_BYTES) {
        let _ = std::fs::rename(log, log.with_extension("jsonl.1"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let Ok(mut file) = options.open(log) else {
        return;
    };
    let at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    for receipt in receipts {
        let line = serde_json::json!({
            "at": at,
            "client": "update_check",
            "allowed": receipt.allowed,
            "dialled": receipt.dialled,
            "reason": receipt.reason,
        });
        let _ = writeln!(file, "{line}");
    }
}

/// Where the notice keeps its files.
fn dir(home: &Path) -> PathBuf {
    home.join("update")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A started (or skipped) check, held until the terminal UI exits.
pub(crate) struct Started {
    /// Why the configuration is unusable, said once at exit.
    problem: Option<String>,
    /// The check's thread, when one was started — kept only for tests to
    /// wait on; a real run lets it go and never waits.
    #[cfg(test)]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Started {
    /// Wait for the check's thread. For tests: the exit path never waits.
    #[cfg(test)]
    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the check ends");
        }
    }
}

/// Begin the check for a terminal UI session, if it is on and due. Never
/// blocks: the network exchange is on a thread of its own. `terminal` is
/// whether there is a terminal to tell, and `env` the session's environment
/// (the process's, in a real run).
pub(crate) fn start(
    home: &Path,
    current: &str,
    terminal: bool,
    env: &[(String, String)],
) -> Started {
    start_with(
        home,
        current,
        terminal,
        &|name| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        },
        now_ms(),
        FETCH_TIMEOUT,
    )
}

/// [`start`] with the terminal, environment, clock and timeout given.
pub(crate) fn start_with(
    home: &Path,
    current: &str,
    terminal: bool,
    env: &dyn Fn(&str) -> Option<String>,
    now_ms: i64,
    timeout: Duration,
) -> Started {
    let config = match load_config(
        Some(&home.join("config.toml")),
        env("RAPIDLM_UPDATE_URL").as_deref(),
    ) {
        Ok(config) => config,
        Err(reason) => {
            return Started {
                problem: Some(format!("update check is off: {reason}")),
                #[cfg(test)]
                thread: None,
            };
        }
    };
    let state_path = dir(home).join("state.json");
    let state = State::load(&state_path);
    if skip_reason(&config, env, terminal, &state, now_ms).is_some() {
        return Started {
            problem: None,
            #[cfg(test)]
            thread: None,
        };
    }
    let Some(url) = config.url else {
        return Started {
            problem: None,
            #[cfg(test)]
            thread: None,
        };
    };
    let current = current.to_owned();
    let receipts_log = dir(home).join("egress-receipts.jsonl");
    let _thread = std::thread::spawn(move || {
        let outcome = check(&url, &current, timeout, &receipts_log);
        // The attempt counts either way; what was found replaces what was
        // known (a server that now says nothing newer clears it).
        let mut state = State::load(&state_path);
        state.checked_at_ms = Some(now_ms);
        if let Ok(found) = outcome {
            state.latest = found;
        }
        state.save(&state_path);
    });
    Started {
        problem: None,
        #[cfg(test)]
        thread: Some(_thread),
    }
}

/// The line to print when the terminal UI has exited, if there is one: a
/// configuration problem, or a newer version not yet announced. Never waits
/// for a check still under way — its result is for the next exit.
pub(crate) fn finish(started: &Started, home: &Path, current: &str) -> Option<String> {
    if let Some(problem) = &started.problem {
        return Some(problem.clone());
    }
    let state_path = dir(home).join("state.json");
    let mut state = State::load(&state_path);
    let latest = state.latest.clone()?;
    if state.announced.as_deref() == Some(latest.as_str()) {
        return None;
    }
    // Only a version that is really newer than this binary.
    crate::update_serve::ensure_newer(current, &latest).ok()?;
    state.announced = Some(latest.clone());
    state.save(&state_path);
    Some(format!(
        "rapid {latest} is available (this is {current}); run `rapid update` to install it"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let path = std::env::temp_dir().join(format!(
                "rapidlm-update-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("dir");
            Self(path)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A one-shot local server answering `body` with `status`, counting what
    /// it was asked. `None` body: accepts and never answers.
    fn server(
        status: u16,
        body: Option<&'static str>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                match body {
                    Some(body) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                    }
                    None => std::thread::sleep(Duration::from_secs(30)),
                }
            }
        });
        (format!("http://127.0.0.1:{port}/rapid/latest.json"), hits)
    }

    fn manifest(version: &str) -> &'static str {
        Box::leak(
            format!(
                r#"{{"version":"{version}","sha256":"{}","url":"https://example.invalid/rapid"}}"#,
                "a".repeat(64)
            )
            .into_boxed_str(),
        )
    }

    fn config_at(home: &Path, body: &str) {
        std::fs::write(home.join("config.toml"), body).expect("config");
    }

    #[test]
    fn the_section_is_read_strictly_and_names_its_own_server() {
        let tmp = Tmp::new("config");
        let load = |body: &str| {
            config_at(&tmp.0, body);
            load_config(Some(&tmp.0.join("config.toml")), None)
        };
        // Nothing, an empty table, and check = false are all off.
        assert_eq!(load("").expect("ok"), UpdateConfig::default());
        assert_eq!(load("[update]\n").expect("ok"), UpdateConfig::default());
        assert!(
            !load("[update]\ncheck = false\nurl = \"https://example.com/m.json\"\n")
                .expect("ok")
                .check
        );
        let on = load("[update]\ncheck = true\nurl = \"https://example.com/rapid/m.json\"\n")
            .expect("ok");
        assert!(on.check);
        assert_eq!(
            on.url.expect("url").url,
            "https://example.com:443/rapid/m.json"
        );
        // Refused: no server, unknown keys, wrong types, unusable addresses.
        for (body, needle) in [
            ("[update]\ncheck = true\n", "needs update.url"),
            (
                "[update]\ncheck = true\nurl = \"https://e.com/m.json\"\nfoo = 1\n",
                "update.foo",
            ),
            ("[update]\ncheck = \"yes\"\n", "true or false"),
            ("[update]\nurl = 3\n", "must be a string"),
            ("update = 3\n", "must be a table"),
            (
                "[update]\nurl = \"http://example.com/m.json\"\n",
                "not an https URL",
            ),
            (
                "[update]\nurl = \"https://example.com\"\n",
                "names no manifest file",
            ),
            (
                "[update]\nurl = \"https://example.com/\"\n",
                "names no manifest file",
            ),
            (
                "[update]\nurl = \"https://u:p@example.com/m.json\"\n",
                "not an https URL",
            ),
            (
                "[update]\nurl = \"https://example.com:99999/m.json\"\n",
                "bad port",
            ),
        ] {
            let err = load(body).expect_err(body);
            assert!(err.contains(needle), "{body}: {err}");
        }
        // A syntax error names the file; a missing file is simply off.
        assert!(load("[update\n").is_err());
        assert_eq!(
            load_config(Some(&tmp.0.join("absent.toml")), None).expect("ok"),
            UpdateConfig::default()
        );
        assert_eq!(
            load_config(None, None).expect("ok"),
            UpdateConfig::default()
        );
        // The environment's URL stands in when the file names none, is
        // outranked by the file's, and is checked like it.
        config_at(&tmp.0, "[update]\ncheck = true\n");
        let path = tmp.0.join("config.toml");
        let from_env = load_config(Some(&path), Some("https://env.example/m.json")).expect("ok");
        assert_eq!(from_env.url.expect("url").host, "env.example");
        assert!(
            load_config(Some(&path), Some("http://env.example/m.json"))
                .expect_err("bad")
                .contains("RAPIDLM_UPDATE_URL")
        );
        assert!(
            load_config(Some(&path), Some("   "))
                .expect_err("blank")
                .contains("needs update.url")
        );
        config_at(
            &tmp.0,
            "[update]\ncheck = true\nurl = \"https://file.example/m.json\"\n",
        );
        let both = load_config(Some(&path), Some("https://env.example/m.json")).expect("ok");
        assert_eq!(both.url.expect("url").host, "file.example");
        // Off, the environment alone changes nothing.
        config_at(&tmp.0, "");
        assert_eq!(
            load_config(Some(&path), Some("https://env.example/m.json")).expect("ok"),
            UpdateConfig::default()
        );
        // http is fine on loopback, for a server the user runs.
        let local =
            load("[update]\ncheck = true\nurl = \"http://127.0.0.1:8080/m.json\"\n").expect("ok");
        assert_eq!(local.url.expect("url").port, 8080);
    }

    #[test]
    fn a_check_is_skipped_off_in_ci_without_a_terminal_when_opted_out_and_within_a_day() {
        let on = UpdateConfig {
            check: true,
            url: Some(ManifestUrl::parse("https://example.com/m.json").expect("url")),
        };
        let fresh = State::default();
        let now = 1_800_000_000_000;
        let skip =
            |config: &UpdateConfig,
             env: &dyn Fn(&str) -> Option<String>,
             terminal,
             state: &State| { skip_reason(config, env, terminal, state, now) };
        assert_eq!(
            skip(&on, &no_env, true, &fresh),
            None,
            "the plain case runs"
        );
        assert_eq!(
            skip(&UpdateConfig::default(), &no_env, true, &fresh),
            Some(Skip::Off)
        );
        assert_eq!(skip(&on, &no_env, false, &fresh), Some(Skip::NoTerminal));
        assert_eq!(
            skip(
                &on,
                &|n| (n == "RAPIDLM_NO_UPDATE_CHECK").then(|| "1".to_owned()),
                true,
                &fresh
            ),
            Some(Skip::OptedOut)
        );
        // Each usual CI variable, and not the ones that say no.
        for name in [
            "CI",
            "GITHUB_ACTIONS",
            "GITLAB_CI",
            "BUILDKITE",
            "TF_BUILD",
            "JENKINS_URL",
            "BUILD_NUMBER",
            "CONTINUOUS_INTEGRATION",
        ] {
            assert_eq!(
                skip(
                    &on,
                    &|n| (n == name).then(|| "true".to_owned()),
                    true,
                    &fresh
                ),
                Some(Skip::Ci),
                "{name}"
            );
        }
        for no in ["", "0", "false", "FALSE", "  "] {
            assert_eq!(
                skip(&on, &|n| (n == "CI").then(|| no.to_owned()), true, &fresh),
                None,
                "CI={no:?}"
            );
        }
        // Throttled inside 24 hours, due at and after it; a clock set back,
        // or a garbage stamp, does not stop the check for a day.
        let checked = |ago: i64| State {
            checked_at_ms: Some(now - ago),
            ..State::default()
        };
        assert_eq!(skip(&on, &no_env, true, &checked(0)), Some(Skip::Throttled));
        assert_eq!(
            skip(&on, &no_env, true, &checked(CHECK_EVERY_MS - 1)),
            Some(Skip::Throttled)
        );
        assert_eq!(skip(&on, &no_env, true, &checked(CHECK_EVERY_MS)), None);
        assert_eq!(
            skip(&on, &no_env, true, &checked(-5_000)),
            None,
            "a stamp in the future"
        );
        assert_eq!(
            skip(&on, &no_env, true, &checked(CHECK_EVERY_MS * 30)),
            None
        );
    }

    #[test]
    fn the_state_survives_a_round_trip_and_garbage_is_a_fresh_start() {
        let tmp = Tmp::new("state");
        let path = tmp.0.join("update").join("state.json");
        assert_eq!(State::load(&path), State::default());
        let state = State {
            checked_at_ms: Some(1_800_000_000_000),
            latest: Some("9.9.9".to_owned()),
            announced: Some("9.9.8".to_owned()),
        };
        state.save(&path);
        assert_eq!(State::load(&path), state);
        // Only the file: no temp file is left behind.
        let names: Vec<String> = std::fs::read_dir(path.parent().expect("dir"))
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["state.json"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o077, 0, "private to the user");
        }
        for garbage in [
            "",
            "not json",
            "[1,2]",
            r#"{"checked_at_ms":"x","latest":7}"#,
        ] {
            std::fs::write(&path, garbage).expect("write");
            assert_eq!(State::load(&path), State::default(), "{garbage}");
        }
        // An absurdly long version is not kept.
        std::fs::write(&path, format!(r#"{{"latest":"{}"}}"#, "9".repeat(500))).expect("write");
        assert_eq!(State::load(&path).latest, None);
    }

    #[test]
    fn a_check_finds_a_newer_release_through_the_gate_and_leaves_a_receipt() {
        let tmp = Tmp::new("check");
        let log = tmp.0.join("update").join("egress-receipts.jsonl");
        let (url, hits) = server(200, Some(manifest("9.9.9")));
        let url = ManifestUrl::parse(&url).expect("url");
        assert_eq!(
            check(&url, "0.1.0", Duration::from_secs(3), &log).expect("checked"),
            Some("9.9.9".to_owned())
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The receipt names what was dialled, and that it was allowed.
        let receipts = std::fs::read_to_string(&log).expect("receipts");
        let line: serde_json::Value =
            serde_json::from_str(receipts.lines().next().expect("a line")).expect("json");
        assert_eq!(line["allowed"], true);
        assert_eq!(line["client"], "update_check");
        assert_eq!(line["dialled"], format!("http://{}:{}", url.host, url.port));
        // The same version, an older one: nothing newer.
        let (same, _) = server(200, Some(manifest("0.1.0")));
        assert_eq!(
            check(
                &ManifestUrl::parse(&same).expect("url"),
                "0.1.0",
                Duration::from_secs(3),
                &log
            )
            .expect("checked"),
            None
        );
        let (older, _) = server(200, Some(manifest("0.0.9")));
        assert_eq!(
            check(
                &ManifestUrl::parse(&older).expect("url"),
                "0.1.0",
                Duration::from_secs(3),
                &log
            )
            .expect("checked"),
            None
        );
    }

    #[test]
    fn a_check_that_cannot_be_trusted_or_reached_is_an_error_not_a_notice() {
        let tmp = Tmp::new("errors");
        let log = tmp.0.join("receipts.jsonl");
        let ask = |status, body: Option<&'static str>| {
            let (url, _) = server(status, body);
            check(
                &ManifestUrl::parse(&url).expect("url"),
                "0.1.0",
                Duration::from_secs(3),
                &log,
            )
        };
        assert!(
            ask(500, Some("boom"))
                .expect_err("500")
                .contains("answered 500")
        );
        assert!(
            ask(200, Some("not json"))
                .expect_err("json")
                .contains("not JSON")
        );
        assert!(
            ask(200, Some(r#"{"version":"9.9.9"}"#))
                .expect_err("fields")
                .contains("missing")
        );
        assert!(ask(200, Some(r#"{"version":"nine","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","url":"https://x/y"}"#))
            .expect_err("not a version")
            .contains("not semantic"));
        // Bigger than the bound: cut off and refused, never read whole.
        let huge: &'static str =
            Box::leak(format!("{{\"pad\":\"{}\"}}", "x".repeat(200_000)).into_boxed_str());
        assert!(ask(200, Some(huge)).is_err());
    }

    #[test]
    fn a_closed_port_and_a_silent_server_cost_at_most_the_timeout() {
        let tmp = Tmp::new("hang");
        let log = tmp.0.join("receipts.jsonl");
        // A port nothing listens on: refused at once.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            drop(listener);
            ManifestUrl::parse(&format!("http://127.0.0.1:{port}/m.json")).expect("url")
        };
        let started = std::time::Instant::now();
        assert!(check(&closed, "0.1.0", Duration::from_secs(3), &log).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // A server that accepts and never answers: the timeout ends it.
        let (silent, _) = server(200, None);
        let started = std::time::Instant::now();
        let err = check(
            &ManifestUrl::parse(&silent).expect("url"),
            "0.1.0",
            Duration::from_millis(600),
            &log,
        )
        .expect_err("times out");
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(500) && took < Duration::from_secs(3),
            "{took:?} {err}"
        );
    }

    #[test]
    fn a_session_checks_once_a_day_and_tells_of_a_new_version_once_at_exit() {
        let tmp = Tmp::new("session");
        let (url, hits) = server(200, Some(manifest("9.9.9")));
        config_at(
            &tmp.0,
            &format!("[update]\ncheck = true\nurl = \"{url}\"\n"),
        );
        let now = 1_800_000_000_000;
        let start = |terminal: bool, at: i64, env: &dyn Fn(&str) -> Option<String>| {
            start_with(&tmp.0, "0.1.0", terminal, env, at, Duration::from_secs(3))
        };
        // No terminal, CI: nothing is asked.
        for (terminal, env) in [
            (false, &no_env as &dyn Fn(&str) -> Option<String>),
            (true, &|n: &str| (n == "CI").then(|| "true".to_owned())),
        ] {
            let mut started = start(terminal, now, env);
            started.join();
            assert!(finish(&started, &tmp.0, "0.1.0").is_none());
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        // A terminal session asks; the answer is for the next exit, and
        // even a quick exit before it lands says nothing.
        let mut first = start(true, now, &no_env);
        first.join();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let line = finish(&first, &tmp.0, "0.1.0").expect("a newer version is known");
        assert!(
            line.contains("9.9.9") && line.contains("0.1.0") && line.contains("rapid update"),
            "{line}"
        );
        // Told once: the next exit is quiet, and a later session within a
        // day does not ask again.
        assert!(finish(&first, &tmp.0, "0.1.0").is_none());
        let mut second = start(true, now + 60_000, &no_env);
        second.join();
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "throttled"
        );
        assert!(finish(&second, &tmp.0, "0.1.0").is_none());
        // A day later it asks again; the same version is not announced twice.
        let mut third = start(true, now + CHECK_EVERY_MS + 1, &no_env);
        third.join();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(finish(&third, &tmp.0, "0.1.0").is_none());
        // Once this binary is that version, nothing is said at all.
        let state_path = tmp.0.join("update").join("state.json");
        let mut state = State::load(&state_path);
        state.announced = None;
        state.save(&state_path);
        assert!(finish(&third, &tmp.0, "9.9.9").is_none());
        assert!(finish(&third, &tmp.0, "10.0.0").is_none());
    }

    #[test]
    fn a_failed_check_counts_and_a_server_that_stops_saying_newer_clears_the_notice() {
        let tmp = Tmp::new("failure");
        let now = 1_800_000_000_000;
        let (url, _) = server(500, Some("boom"));
        config_at(
            &tmp.0,
            &format!("[update]\ncheck = true\nurl = \"{url}\"\n"),
        );
        let mut failed = start_with(&tmp.0, "0.1.0", true, &no_env, now, Duration::from_secs(3));
        failed.join();
        let state = State::load(&tmp.0.join("update").join("state.json"));
        assert_eq!(
            state.checked_at_ms,
            Some(now),
            "the attempt counts, so a broken server is not hammered"
        );
        assert_eq!(state.latest, None);
        assert!(finish(&failed, &tmp.0, "0.1.0").is_none());
        // A known newer version, then a server that no longer says so.
        State {
            checked_at_ms: Some(now - CHECK_EVERY_MS - 1),
            latest: Some("9.9.9".to_owned()),
            announced: None,
        }
        .save(&tmp.0.join("update").join("state.json"));
        let (url, _) = server(200, Some(manifest("0.1.0")));
        config_at(
            &tmp.0,
            &format!("[update]\ncheck = true\nurl = \"{url}\"\n"),
        );
        let mut later = start_with(&tmp.0, "0.1.0", true, &no_env, now, Duration::from_secs(3));
        later.join();
        assert_eq!(
            State::load(&tmp.0.join("update").join("state.json")).latest,
            None
        );
        assert!(finish(&later, &tmp.0, "0.1.0").is_none());
    }

    #[test]
    fn a_broken_configuration_is_said_once_at_exit_and_nothing_is_asked() {
        let tmp = Tmp::new("broken");
        config_at(&tmp.0, "[update]\ncheck = true\n");
        let started = start_with(&tmp.0, "0.1.0", true, &no_env, 1, Duration::from_secs(3));
        let line = finish(&started, &tmp.0, "0.1.0").expect("a problem line");
        assert!(
            line.contains("update check is off") && line.contains("update.url"),
            "{line}"
        );
        assert!(!tmp.0.join("update").exists(), "nothing was written");
    }

    #[test]
    fn a_disabled_check_touches_nothing() {
        let tmp = Tmp::new("off");
        let started = start_with(&tmp.0, "0.1.0", true, &no_env, 1, Duration::from_secs(3));
        assert!(started.thread.is_none());
        assert!(finish(&started, &tmp.0, "0.1.0").is_none());
        assert!(!tmp.0.join("update").exists());
    }
}
