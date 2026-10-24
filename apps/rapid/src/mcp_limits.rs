//! The time an MCP server gets (EXT-01 §3.2): to start, and to answer a call.
//!
//! Each limit is resolved from layers, the highest that says anything winning:
//! the environment over the user's `[mcp]` config over the server's own key in
//! the project's settings over the built-in, and a managed ceiling over all of
//! them for the call limit. A project's per-server value is the *lowest*
//! configured layer: a user who needs longer for a project's server says so in
//! their own config. The built-ins are what the build has always done (thirty
//! seconds for each), so a project that says nothing sees no change.

use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Duration;

use crate::managed_config::{ConfigOrigin, GateReportEntry};

/// The bounds a start-up limit may be given, in milliseconds.
pub const STARTUP_RANGE_MS: RangeInclusive<u64> = 1_000..=120_000;
/// The bounds a call limit may be given, in milliseconds.
pub const TOOL_RANGE_MS: RangeInclusive<u64> = 1_000..=600_000;
/// What each limit is when nothing says otherwise.
pub const DEFAULT_STARTUP_MS: u64 = 30_000;
pub const DEFAULT_TOOL_MS: u64 = 30_000;

/// The environment variables that set the two limits for every server.
pub const STARTUP_ENV: &str = "RAPIDLM_MCP_STARTUP_TIMEOUT_MS";
pub const TOOL_ENV: &str = "RAPIDLM_MCP_TOOL_TIMEOUT_MS";

/// The most of the user's config file that is read for `[mcp]`.
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// The process variables [`Layers::load`] reads, from an environment `get`
/// reads: the two limits and what finds the user's config file.
pub fn env_from(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<(String, String)> {
    [
        STARTUP_ENV,
        TOOL_ENV,
        crate::user_config::CONFIG_PATH_ENV,
        crate::user_config::RAPIDLM_HOME_ENV,
        crate::user_config::HOME_ENV,
        crate::user_config::USERPROFILE_ENV,
    ]
    .iter()
    .filter_map(|key| {
        get(key).map(|value| ((*key).to_owned(), value.to_string_lossy().into_owned()))
    })
    .collect()
}

/// Which layer a resolved limit came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layer {
    BuiltIn,
    Server,
    User,
    Env,
    Managed,
}

impl Layer {
    /// The words a report uses.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Server => "project settings",
            Self::User => "user config [mcp]",
            Self::Env => "environment",
            Self::Managed => "managed policy",
        }
    }
}

/// What one layer asks for: `None` is "nothing said".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Requested {
    pub startup_ms: Option<u64>,
    pub tool_ms: Option<u64>,
}

/// A server's resolved limits, and where each came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McpLimits {
    startup_ms: u64,
    tool_ms: u64,
    startup_from: Layer,
    tool_from: Layer,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            startup_ms: DEFAULT_STARTUP_MS,
            tool_ms: DEFAULT_TOOL_MS,
            startup_from: Layer::BuiltIn,
            tool_from: Layer::BuiltIn,
        }
    }
}

impl McpLimits {
    /// How long a server gets for each wait while it starts: the handshake,
    /// then the tool listing.
    #[must_use]
    pub fn startup(&self) -> Duration {
        Duration::from_millis(self.startup_ms)
    }

    /// How long a server gets to answer one call.
    #[must_use]
    pub fn tool(&self) -> Duration {
        Duration::from_millis(self.tool_ms)
    }

    #[must_use]
    pub fn startup_ms(&self) -> u64 {
        self.startup_ms
    }

    #[must_use]
    pub fn tool_ms(&self) -> u64 {
        self.tool_ms
    }

    #[must_use]
    pub fn startup_from(&self) -> Layer {
        self.startup_from
    }

    #[must_use]
    pub fn tool_from(&self) -> Layer {
        self.tool_from
    }
}

/// The limits of one server, and whether the managed ceiling changed them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resolved {
    pub limits: McpLimits,
    /// The call limit the ceiling lowered, as it was before.
    pub lowered_from_ms: Option<u64>,
}

/// Every layer above a server's own entry: the user's config, the
/// environment and the managed ceiling, read once.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Layers {
    user: Requested,
    env: Requested,
    ceiling_ms: Option<u64>,
    problems: Vec<String>,
}

impl Layers {
    /// The layers from an environment (see [`env_from`]) and the managed
    /// ceiling. A value that is not usable is left out and named in
    /// [`Self::problems`]: a limit that silently did not apply would leave a
    /// server running on a bound the person did not choose.
    #[must_use]
    pub fn load(
        env: &[(String, String)],
        explicit_home: Option<&Path>,
        ceiling_ms: Option<u64>,
    ) -> Self {
        let mut problems = Vec::new();
        let from_env = Requested {
            startup_ms: env_value(env, STARTUP_ENV, &STARTUP_RANGE_MS, &mut problems),
            tool_ms: env_value(env, TOOL_ENV, &TOOL_RANGE_MS, &mut problems),
        };
        // A config named by a relative path is not read for this: it would be
        // whatever the working directory holds, a project's own files included.
        if let Some(named) = env
            .iter()
            .find(|(key, _)| key == crate::user_config::CONFIG_PATH_ENV)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.trim().is_empty() && !Path::new(value).is_absolute())
        {
            problems.push(format!(
                "{} is a relative path ({}): [mcp] is not read from it",
                crate::user_config::CONFIG_PATH_ENV,
                crate::mcp_config::label(named)
            ));
        }
        let user = match crate::skills_runtime::config_path(explicit_home, env) {
            Some(path) => user_layer(&path, &mut problems),
            None => Requested::default(),
        };
        Self {
            user,
            env: from_env,
            ceiling_ms,
            problems,
        }
    }

    /// What was wrong with a value in the environment or the user's config.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// The managed policy's longest a call may be given.
    #[must_use]
    pub fn ceiling_ms(&self) -> Option<u64> {
        self.ceiling_ms
    }

    /// A report for each of `servers` whose call limit the managed ceiling
    /// lowered.
    #[must_use]
    pub fn ceiling_reports<'a>(
        &self,
        servers: impl IntoIterator<Item = &'a crate::exec_tools::McpServerConfig>,
    ) -> Vec<GateReportEntry> {
        servers
            .into_iter()
            .filter_map(|server| {
                let resolved = self.resolve(server.timeouts);
                Some(ceiling_report(
                    &server.name,
                    resolved.lowered_from_ms?,
                    resolved.limits.tool_ms(),
                ))
            })
            .collect()
    }

    /// The limits of a server whose own entry asks for `server`.
    #[must_use]
    pub fn resolve(&self, server: Requested) -> Resolved {
        let pick = |layers: [(Option<u64>, Layer); 3], built_in: u64| {
            layers
                .into_iter()
                .find_map(|(ms, layer)| ms.map(|ms| (ms, layer)))
                .unwrap_or((built_in, Layer::BuiltIn))
        };
        let (startup_ms, startup_from) = pick(
            [
                (self.env.startup_ms, Layer::Env),
                (self.user.startup_ms, Layer::User),
                (server.startup_ms, Layer::Server),
            ],
            DEFAULT_STARTUP_MS,
        );
        let (mut tool_ms, mut tool_from) = pick(
            [
                (self.env.tool_ms, Layer::Env),
                (self.user.tool_ms, Layer::User),
                (server.tool_ms, Layer::Server),
            ],
            DEFAULT_TOOL_MS,
        );
        let mut lowered_from_ms = None;
        if let Some(ceiling) = self.ceiling_ms
            && tool_ms > ceiling
        {
            lowered_from_ms = Some(tool_ms);
            tool_ms = ceiling;
            tool_from = Layer::Managed;
        }
        Resolved {
            limits: McpLimits {
                startup_ms,
                tool_ms,
                startup_from,
                tool_from,
            },
            lowered_from_ms,
        }
    }
}

/// A value read from an environment variable: unset or blank is nothing said;
/// anything else must be a whole number of milliseconds in `range`.
fn env_value(
    env: &[(String, String)],
    name: &str,
    range: &RangeInclusive<u64>,
    problems: &mut Vec<String>,
) -> Option<u64> {
    let raw = env
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())?;
    match raw.parse::<u64>().ok().filter(|ms| range.contains(ms)) {
        Some(ms) => Some(ms),
        None => {
            problems.push(format!(
                "{name} must be a whole number of milliseconds from {} to {}: ignored",
                range.start(),
                range.end()
            ));
            None
        }
    }
}

/// The `[mcp]` limits of the config file at `path`.
fn user_layer(path: &Path, problems: &mut Vec<String>) -> Requested {
    use std::io::Read;
    // A file, not a pipe or a device that would block the read.
    if !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return Requested::default();
    }
    let Ok(file) = std::fs::File::open(path) else {
        return Requested::default();
    };
    let mut text = String::new();
    if file
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .is_err()
    {
        return Requested::default();
    }
    if text.len() as u64 > MAX_CONFIG_BYTES {
        problems.push(format!(
            "the config is over {MAX_CONFIG_BYTES} bytes: [mcp] is not read"
        ));
        return Requested::default();
    }
    // A file that is not TOML is reported where the config is loaded.
    let Ok(parsed) = toml::from_str::<toml::Value>(&text) else {
        return Requested::default();
    };
    let Some(table) = parsed.get("mcp") else {
        return Requested::default();
    };
    let Some(table) = table.as_table() else {
        problems.push("[mcp] must be a table: ignored".to_owned());
        return Requested::default();
    };
    let mut read = |key: &str, range: &RangeInclusive<u64>| {
        let value = table.get(key)?;
        let ms = value
            .as_integer()
            .and_then(|ms| u64::try_from(ms).ok())
            .filter(|ms| range.contains(ms));
        if ms.is_none() {
            problems.push(format!(
                "[mcp] {key} must be a whole number of milliseconds from {} to {}: ignored",
                range.start(),
                range.end()
            ));
        }
        ms
    };
    Requested {
        startup_ms: read("startup_timeout_ms", &STARTUP_RANGE_MS),
        tool_ms: read("tool_timeout_ms", &TOOL_RANGE_MS),
    }
}

/// The report for a call limit the managed ceiling lowered.
#[must_use]
pub fn ceiling_report(server: &str, from_ms: u64, to_ms: u64) -> GateReportEntry {
    GateReportEntry {
        field_id: "mcp.max_tool_timeout_ms".to_owned(),
        origin: ConfigOrigin::Managed,
        detail: format!(
            "{server}'s call limit lowered from {from_ms} ms to {to_ms} ms; the managed policy sets the longest a call may take"
        ),
        remediation: "lower tool_timeout_ms, or ask your administrator to raise max_tool_timeout_ms",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn ms(startup: Option<u64>, tool: Option<u64>) -> Requested {
        Requested {
            startup_ms: startup,
            tool_ms: tool,
        }
    }

    /// A config file holding `body`, found through `RAPIDLM_CONFIG`.
    struct Config(std::path::PathBuf);

    impl Config {
        fn new(label: &str, body: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rapid-mcp-limits-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join("config.toml");
            std::fs::write(&path, body).expect("config");
            Self(path)
        }

        fn env(&self, more: &[(&str, &str)]) -> Vec<(String, String)> {
            let mut pairs = env(more);
            pairs.push((
                crate::user_config::CONFIG_PATH_ENV.to_owned(),
                self.0.display().to_string(),
            ));
            pairs
        }
    }

    impl Drop for Config {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    #[test]
    fn with_nothing_said_the_limits_are_what_the_build_always_did() {
        let layers = Layers::load(&[], None, None);
        let resolved = layers.resolve(Requested::default());
        assert_eq!(resolved.limits, McpLimits::default());
        assert_eq!(resolved.limits.startup(), Duration::from_secs(30));
        assert_eq!(resolved.limits.tool(), Duration::from_secs(30));
        assert_eq!(resolved.limits.startup_from(), Layer::BuiltIn);
        assert_eq!(resolved.limits.tool_from(), Layer::BuiltIn);
        assert!(layers.problems().is_empty() && resolved.lowered_from_ms.is_none());
    }

    #[test]
    fn the_highest_layer_that_says_anything_wins() {
        let server = ms(Some(5_000), Some(50_000));
        let config = Config::new(
            "order",
            "[mcp]\nstartup_timeout_ms = 6000\ntool_timeout_ms = 60000\n",
        );
        let at = |layers: &Layers, name: &str| {
            let limits = layers.resolve(server).limits;
            match name {
                "startup" => (limits.startup_ms(), limits.startup_from()),
                _ => (limits.tool_ms(), limits.tool_from()),
            }
        };
        // Each layer in turn takes over from the one below it.
        let server_only = Layers::load(&[], None, None);
        assert_eq!(at(&server_only, "startup"), (5_000, Layer::Server));
        assert_eq!(at(&server_only, "tool"), (50_000, Layer::Server));
        let with_user = Layers::load(&config.env(&[]), None, None);
        assert_eq!(at(&with_user, "startup"), (6_000, Layer::User));
        assert_eq!(at(&with_user, "tool"), (60_000, Layer::User));
        let with_env = Layers::load(
            &config.env(&[(STARTUP_ENV, "7000"), (TOOL_ENV, "70000")]),
            None,
            None,
        );
        assert_eq!(at(&with_env, "startup"), (7_000, Layer::Env));
        assert_eq!(at(&with_env, "tool"), (70_000, Layer::Env));
        // The two limits are resolved on their own.
        let split = Config::new("split", "[mcp]\ntool_timeout_ms = 60000\n");
        let layers = Layers::load(&split.env(&[]), None, None);
        let limits = layers.resolve(ms(Some(5_000), None)).limits;
        assert_eq!(limits.startup_from(), Layer::Server);
        assert_eq!(limits.tool_from(), Layer::User);
    }

    #[test]
    fn the_managed_ceiling_lowers_a_larger_call_limit_and_says_from_what() {
        let layers = Layers::load(&env(&[(TOOL_ENV, "200000")]), None, Some(120_000));
        let resolved = layers.resolve(ms(None, Some(300_000)));
        assert_eq!(resolved.limits.tool_ms(), 120_000);
        assert_eq!(resolved.limits.tool_from(), Layer::Managed);
        assert_eq!(
            resolved.lowered_from_ms,
            Some(200_000),
            "the value the environment asked for"
        );
        // A value at or under the ceiling is left alone, and so is the start-up limit.
        let layers = Layers::load(&[], None, Some(120_000));
        let resolved = layers.resolve(ms(Some(100_000), Some(120_000)));
        assert_eq!(
            (resolved.limits.tool_ms(), resolved.limits.tool_from()),
            (120_000, Layer::Server)
        );
        assert_eq!(resolved.limits.startup_ms(), 100_000);
        assert!(resolved.lowered_from_ms.is_none());
        // A low ceiling lowers the built-in too.
        let resolved = Layers::load(&[], None, Some(10_000)).resolve(Requested::default());
        assert_eq!(
            (resolved.limits.tool_ms(), resolved.lowered_from_ms),
            (10_000, Some(30_000))
        );
    }

    #[test]
    fn a_bad_environment_value_is_named_and_ignored() {
        for bad in ["abc", "0", "999", "120001", "-5", "1.5", " 12ms "] {
            let layers = Layers::load(&env(&[(STARTUP_ENV, bad)]), None, None);
            let limits = layers.resolve(ms(Some(5_000), None)).limits;
            assert_eq!(limits.startup_from(), Layer::Server, "{bad:?}");
            assert_eq!(layers.problems().len(), 1, "{bad:?}");
            assert!(layers.problems()[0].contains(STARTUP_ENV), "{bad:?}");
        }
        // The call limit goes to 600 s, the start-up limit only to 120 s; blank is unset.
        let layers = Layers::load(
            &env(&[(TOOL_ENV, "600000"), (STARTUP_ENV, "  ")]),
            None,
            None,
        );
        let limits = layers.resolve(Requested::default()).limits;
        assert_eq!(limits.tool_ms(), 600_000);
        assert_eq!(limits.startup_from(), Layer::BuiltIn);
        assert!(layers.problems().is_empty(), "{:?}", layers.problems());
        let layers = Layers::load(&env(&[(TOOL_ENV, "600001")]), None, None);
        assert_eq!(layers.problems().len(), 1);
    }

    #[test]
    fn a_bad_user_config_value_is_named_and_the_other_key_still_applies() {
        let config = Config::new(
            "bad-user",
            "[mcp]\nstartup_timeout_ms = 5\ntool_timeout_ms = 45000\n",
        );
        let layers = Layers::load(&config.env(&[]), None, None);
        let limits = layers.resolve(ms(Some(9_000), None)).limits;
        assert_eq!(
            (limits.startup_ms(), limits.startup_from()),
            (9_000, Layer::Server)
        );
        assert_eq!(
            (limits.tool_ms(), limits.tool_from()),
            (45_000, Layer::User)
        );
        assert_eq!(layers.problems().len(), 1, "{:?}", layers.problems());
        assert!(layers.problems()[0].contains("startup_timeout_ms"));
        // A float, a string and a table are not whole numbers; a missing table is nothing said.
        for body in [
            "[mcp]\ntool_timeout_ms = 1.5e4\n",
            "[mcp]\ntool_timeout_ms = \"60000\"\n",
            "mcp = 3\n",
        ] {
            let config = Config::new("odd-user", body);
            let layers = Layers::load(&config.env(&[]), None, None);
            assert_eq!(layers.problems().len(), 1, "{body:?}");
            assert_eq!(
                layers.resolve(Requested::default()).limits,
                McpLimits::default()
            );
        }
        let config = Config::new("none", "[skills]\nextra_dirs = []\n");
        assert!(
            Layers::load(&config.env(&[]), None, None)
                .problems()
                .is_empty()
        );
    }

    #[test]
    fn only_the_variables_the_limits_need_are_taken_from_the_process() {
        let wanted = [
            STARTUP_ENV,
            TOOL_ENV,
            "RAPIDLM_CONFIG",
            "RAPIDLM_HOME",
            "HOME",
            "USERPROFILE",
        ];
        let got = env_from(|key| Some(std::ffi::OsString::from(format!("v-{key}"))));
        let names: Vec<&str> = got.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(names, wanted);
        // An unset variable is absent, and a value that is not Unicode is read lossily.
        assert!(env_from(|_| None).is_empty());
        let odd = env_from(|key| (key == TOOL_ENV).then(|| std::ffi::OsString::from("5000")));
        assert_eq!(odd, [(TOOL_ENV.to_owned(), "5000".to_owned())]);
    }

    #[cfg(unix)]
    #[test]
    fn a_config_that_is_not_a_file_is_not_read_and_does_not_block() {
        // A pipe at the config path would block a read forever.
        let dir =
            std::env::temp_dir().join(format!("rapid-mcp-limits-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let fifo = dir.join("config.toml");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|status| status.success()) {
            return; // no mkfifo here: nothing to show
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let env = vec![(
            crate::user_config::CONFIG_PATH_ENV.to_owned(),
            fifo.display().to_string(),
        )];
        std::thread::spawn(move || {
            let layers = Layers::load(&env, None, None);
            let _ = tx.send(layers.resolve(Requested::default()).limits);
        });
        let limits = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("loading the layers did not block on the pipe");
        assert_eq!(limits, McpLimits::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_relative_config_path_is_not_read_and_says_so() {
        let layers = Layers::load(&env(&[("RAPIDLM_CONFIG", "config.toml")]), None, None);
        assert_eq!(layers.problems().len(), 1, "{:?}", layers.problems());
        assert!(
            layers.problems()[0].contains("RAPIDLM_CONFIG is a relative path"),
            "{:?}",
            layers.problems()
        );
        // Blank is not a path, and an absolute one is not a problem.
        assert!(
            Layers::load(&env(&[("RAPIDLM_CONFIG", "  ")]), None, None)
                .problems()
                .is_empty()
        );
    }

    #[test]
    fn the_report_names_the_field_origin_and_way_out() {
        let report = ceiling_report("tracker", 300_000, 120_000);
        assert_eq!(report.field_id, "mcp.max_tool_timeout_ms");
        assert_eq!(report.origin, ConfigOrigin::Managed);
        assert!(report.detail.contains("tracker") && report.detail.contains("300000"));
        assert!(report.remediation.contains("max_tool_timeout_ms"));
    }
}
