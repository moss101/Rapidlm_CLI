//! Agent-mode flags (SEAM-10): `--output json|text`, `--quiet`,
//! `--non-interactive`, `--dry-run`, `--yes`, `--no-color` and
//! `--timeout <secs>`, given before the subcommand (`rapid --dry-run
//! worktree reclaim`) or through their `RAPIDLM_*` mirrors, parsed once
//! before dispatch.
//!
//! A flag is never silently ignored. Each command declares what it has
//! ([`forms`]): a flag it has natively is passed to it; `--dry-run` on a
//! command that only reads is accepted (there is nothing to not do); a flag
//! a command cannot honour — `--dry-run` on one that changes things,
//! `--non-interactive` on the TUI — is a usage error before anything runs.

use std::sync::OnceLock;
use std::time::Duration;

/// `--output`: how a command's result is written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Json,
    Text,
}

/// The agent-mode flags one invocation runs under.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentMode {
    pub output: Option<Output>,
    pub quiet: bool,
    pub non_interactive: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub no_color: bool,
    pub timeout: Option<Duration>,
}

static CURRENT: OnceLock<AgentMode> = OnceLock::new();

/// The flags this process runs under (none set before dispatch parses them).
pub fn current() -> &'static AgentMode {
    static NONE: AgentMode = AgentMode {
        output: None,
        quiet: false,
        non_interactive: false,
        dry_run: false,
        yes: false,
        no_color: false,
        timeout: None,
    };
    CURRENT.get().unwrap_or(&NONE)
}

/// Record the flags for this process; the first call wins.
pub fn install(mode: AgentMode) {
    let _ = CURRENT.set(mode);
}

/// The longest `--timeout` accepted: a day.
pub const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn parse_output(raw: &str) -> Result<Output, String> {
    match raw {
        "json" => Ok(Output::Json),
        "text" => Ok(Output::Text),
        other => Err(format!("--output takes json or text, not '{other}'")),
    }
}

fn parse_timeout(raw: &str) -> Result<Duration, String> {
    let secs: u64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("--timeout takes whole seconds, not '{raw}'"))?;
    if !(1..=MAX_TIMEOUT_SECS).contains(&secs) {
        return Err(format!("--timeout takes 1 to {MAX_TIMEOUT_SECS} seconds"));
    }
    Ok(Duration::from_secs(secs))
}

/// Read the agent-mode flags before the subcommand name in `args`, over
/// their environment mirrors (a flag wins over its mirror). Returns the
/// mode and the index of the first argument that is not one of them — the
/// subcommand, or an argument this parser does not own (left to the
/// dispatcher as before).
pub fn parse(args: &[String], env: &[(String, String)]) -> Result<(AgentMode, usize), String> {
    let var = |name: &str| {
        env.iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let mut mode = AgentMode {
        output: var("RAPIDLM_OUTPUT")
            .filter(|value| !value.is_empty())
            .map(parse_output)
            .transpose()?,
        quiet: var("RAPIDLM_QUIET").is_some_and(truthy),
        non_interactive: var("RAPIDLM_NON_INTERACTIVE").is_some_and(truthy),
        dry_run: var("RAPIDLM_DRY_RUN").is_some_and(truthy),
        yes: var("RAPIDLM_YES").is_some_and(truthy),
        // The common `NO_COLOR` convention counts when set to anything.
        no_color: var("RAPIDLM_NO_COLOR").is_some_and(truthy)
            || var("NO_COLOR").is_some_and(|value| !value.is_empty()),
        timeout: var("RAPIDLM_TIMEOUT")
            .filter(|value| !value.is_empty())
            .map(parse_timeout)
            .transpose()?,
    };
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--quiet" => mode.quiet = true,
            "--non-interactive" => mode.non_interactive = true,
            "--dry-run" => mode.dry_run = true,
            "--yes" => mode.yes = true,
            "--no-color" => mode.no_color = true,
            "--output" | "--timeout" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{arg} needs a value"))?;
                if arg == "--output" {
                    mode.output = Some(parse_output(value)?);
                } else {
                    mode.timeout = Some(parse_timeout(value)?);
                }
                index += 1;
            }
            other => {
                if let Some(value) = other.strip_prefix("--output=") {
                    mode.output = Some(parse_output(value)?);
                } else if let Some(value) = other.strip_prefix("--timeout=") {
                    mode.timeout = Some(parse_timeout(value)?);
                } else {
                    break;
                }
            }
        }
        index += 1;
    }
    Ok((mode, index))
}

/// What `--dry-run` means for a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DryRun {
    /// The command has its own dry run: these arguments ask for it.
    Native(&'static [&'static str]),
    /// The command only reads: there is nothing to not do.
    ReadOnly,
    /// The command changes things and has no dry run: refused.
    Unsupported,
}

/// What a command has for the agent-mode flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Forms {
    /// Arguments that ask the command for its JSON form, if it has one.
    pub json: Option<&'static [&'static str]>,
    /// Arguments that ask it to be quiet, if it can be.
    pub quiet: Option<&'static [&'static str]>,
    pub dry_run: DryRun,
    /// A command that opens the interactive TUI: refused under
    /// `--non-interactive`.
    pub interactive: bool,
}

const NONE: Forms = Forms {
    json: None,
    quiet: None,
    dry_run: DryRun::Unsupported,
    interactive: false,
};

/// Each subcommand's forms. A command not named here has none: `--dry-run`
/// on it is refused.
pub fn forms(command: &str) -> Forms {
    const OUTPUT_JSON: &[&str] = &["--output", "json"];
    match command {
        "usage" => Forms {
            json: Some(OUTPUT_JSON),
            quiet: Some(&["--quiet"]),
            dry_run: DryRun::ReadOnly,
            ..NONE
        },
        "du" => Forms {
            json: Some(OUTPUT_JSON),
            dry_run: DryRun::ReadOnly,
            ..NONE
        },
        "setup" => Forms {
            json: Some(OUTPUT_JSON),
            dry_run: DryRun::Native(&["--dry-run"]),
            ..NONE
        },
        "browser" => Forms {
            json: Some(&["--json"]),
            ..NONE
        },
        "worktree" | "mcp" => Forms {
            dry_run: DryRun::Native(&["--dry-run"]),
            ..NONE
        },
        "doctor" | "sessions" | "insights" | "tools" | "mcp-tools" | "completions" | "man" => {
            Forms {
                dry_run: DryRun::ReadOnly,
                ..NONE
            }
        }
        "resume" => Forms {
            interactive: true,
            ..NONE
        },
        _ => NONE,
    }
}

/// The operands `command` runs with under `mode`, or the usage error that
/// stops it before anything runs.
pub fn apply(
    command: &str,
    mode: &AgentMode,
    operands: &[String],
    stdout_is_terminal: bool,
) -> Result<Vec<String>, String> {
    let forms = forms(command);
    let mut operands = operands.to_vec();
    let extend = |operands: &mut Vec<String>, args: &[&str]| {
        operands.extend(args.iter().map(|arg| (*arg).to_owned()));
    };
    if mode.non_interactive && forms.interactive {
        return Err(format!(
            "rapid {command} opens the interactive TUI, which --non-interactive refuses"
        ));
    }
    if mode.dry_run {
        match forms.dry_run {
            DryRun::Native(args) => extend(&mut operands, args),
            DryRun::ReadOnly => {}
            DryRun::Unsupported => {
                return Err(format!(
                    "rapid {command} changes state and has no dry run; nothing was done"
                ));
            }
        }
    }
    // JSON where the command has it: asked for, or — in agent mode
    // (`--non-interactive`) — by default when stdout is not a terminal and
    // the command was not told otherwise. Outside agent mode a pipe keeps
    // the text it always had.
    let explicit = operands
        .iter()
        .any(|arg| arg == "--output" || arg.starts_with("--output=") || arg == "--json");
    let wants_json = match mode.output {
        Some(Output::Json) => true,
        Some(Output::Text) => false,
        None => mode.non_interactive && !stdout_is_terminal,
    };
    if wants_json
        && !explicit
        && let Some(args) = forms.json
    {
        extend(&mut operands, args);
    }
    if mode.quiet
        && let Some(args) = forms.quiet
        && !operands.iter().any(|arg| arg == args[0])
    {
        extend(&mut operands, args);
    }
    Ok(operands)
}

/// Exit the process with the interrupted code once `limit` has passed —
/// `--timeout`. The run is cut short, not waited for.
pub fn start_watchdog(limit: Duration) {
    let _ = std::thread::Builder::new()
        .name("rapidlm-timeout".to_owned())
        .spawn(move || {
            std::thread::sleep(limit);
            eprintln!("rapid: --timeout of {}s reached; stopping", limit.as_secs());
            std::process::exit(crate::headless::jsonl::JsonlExitCode::Interrupted.as_i32());
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn flags_come_before_the_command_and_win_over_their_mirrors() {
        let env = vec![
            ("RAPIDLM_OUTPUT".to_owned(), "text".to_owned()),
            ("RAPIDLM_DRY_RUN".to_owned(), "1".to_owned()),
            ("NO_COLOR".to_owned(), "x".to_owned()),
        ];
        let (mode, at) = parse(
            &args(&[
                "--output",
                "json",
                "--timeout=5",
                "--quiet",
                "usage",
                "--quiet",
            ]),
            &env,
        )
        .expect("parse");
        assert_eq!(at, 4);
        assert_eq!(mode.output, Some(Output::Json));
        assert_eq!(mode.timeout, Some(Duration::from_secs(5)));
        assert!(mode.quiet && mode.dry_run && mode.no_color);
        assert!(parse(&args(&["--output", "yaml"]), &[]).is_err());
        assert!(parse(&args(&["--timeout", "0"]), &[]).is_err());
        assert!(parse(&args(&["--timeout"]), &[]).is_err());
        let bad = vec![("RAPIDLM_TIMEOUT".to_owned(), "soon".to_owned())];
        assert!(parse(&args(&["exec"]), &bad).is_err());
    }

    #[test]
    fn every_subcommand_honours_or_refuses_each_flag() {
        let dry = AgentMode {
            dry_run: true,
            ..AgentMode::default()
        };
        let json = AgentMode {
            output: Some(Output::Json),
            quiet: true,
            ..AgentMode::default()
        };
        let agent = AgentMode {
            non_interactive: true,
            ..AgentMode::default()
        };
        for entry in crate::interactive::SUBCOMMANDS.iter() {
            let name = entry.name;
            let forms = forms(name);
            match (forms.dry_run, apply(name, &dry, &[], true)) {
                (DryRun::Native(native), Ok(ops)) => assert_eq!(ops, args(native), "{name}"),
                (DryRun::ReadOnly, Ok(ops)) => assert!(ops.is_empty(), "{name}"),
                (DryRun::Unsupported, Err(reason)) => {
                    assert!(reason.contains(name), "{name}: {reason}")
                }
                (want, got) => panic!("{name}: {want:?} gave {got:?}"),
            }
            let ops = apply(name, &json, &[], true).expect(name);
            let mut expected = forms.json.map(args).unwrap_or_default();
            expected.extend(forms.quiet.map(args).unwrap_or_default());
            assert_eq!(ops, expected, "{name}");
            // An explicit output form in the operands is left alone.
            if forms.json.is_some() {
                let ops = apply(name, &json, &args(&["--output", "text"]), true).expect(name);
                assert!(
                    !ops.contains(&"json".to_owned()) || name == "browser",
                    "{name}"
                );
            }
            let refused = apply(name, &agent, &[], false);
            assert_eq!(refused.is_err(), forms.interactive, "{name}");
            // Agent mode, stdout a pipe: JSON where the command has it.
            if let Ok(ops) = refused {
                assert_eq!(!ops.is_empty(), forms.json.is_some(), "{name}");
            }
            // Outside agent mode a pipe keeps the text it always had.
            assert!(
                apply(name, &AgentMode::default(), &[], false)
                    .expect(name)
                    .is_empty(),
                "{name}"
            );
        }
    }
}
