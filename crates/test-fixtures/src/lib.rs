//! Fixture programs for tests that drive real OS processes.
//!
//! The process-supervision, hook, sandbox and job tests exercise real
//! children — `echo`, `sleep`, `cat`, `sh -c …` — because a fake child
//! cannot prove that stdout is drained past the pipe buffer, that a timeout
//! reaches a grandchild, or that a hook receives its stdin payload. Those
//! programs live at `/bin/…` on Unix; on Windows the same POSIX tools ship
//! with Git for Windows (`C:\Program Files\Git\usr\bin\*.exe`), which every
//! Windows host running this product already has (the workspace backend
//! is `git worktree`). This crate resolves a tool by name to an absolute
//! path on the current host so a test names *what* it runs, not *where*
//! the host keeps it.
//!
//! Resolution is deliberately not a `$PATH` search of the product's own
//! resolvers — those fail closed on relative names by design — but the
//! tests' own lookup, done once, with the result handed to the product as
//! the absolute path it requires.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Where Git for Windows installs its POSIX userland, in preference order.
#[cfg(windows)]
const GIT_USR_BIN: &[&str] = &[
    r"C:\Program Files\Git\usr\bin",
    r"C:\Program Files (x86)\Git\usr\bin",
];

/// Where a POSIX host keeps the fixture tools, in preference order.
#[cfg(not(windows))]
const UNIX_BIN: &[&str] = &["/bin", "/usr/bin"];

/// Absolute path of the fixture tool `name` (`echo`, `sleep`, `cat`, `dd`,
/// `false`, `true`, `printf`, `env`, `sh`, …) on this host.
///
/// Returns `None` when the host has no such tool; [`tool`] is the
/// panicking form for tests that cannot run without it.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    assert!(
        !name.is_empty() && !name.contains(['/', '\\']),
        "fixture tool is a bare program name, got {name:?}"
    );
    #[cfg(windows)]
    {
        let file = format!("{name}.exe");
        for dir in GIT_USR_BIN {
            let candidate = Path::new(dir).join(&file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        // A Git installed elsewhere (or a standalone MSYS2) puts its
        // userland on PATH; take the first hit that is a real file.
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(&file))
            .find(|candidate| candidate.is_file())
    }
    #[cfg(not(windows))]
    {
        UNIX_BIN
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .find(|candidate| candidate.is_file())
    }
}

/// [`find_tool`] or a panic naming the missing tool and where it was
/// looked for.
pub fn tool(name: &str) -> PathBuf {
    find_tool(name)
        .unwrap_or_else(|| panic!("missing test fixture tool {name:?}: {}", where_tools_live()))
}

/// [`tool`] as the `/`-separated string form the product's canonical host
/// path types accept (`C:/Program Files/Git/usr/bin/echo.exe` on Windows,
/// `/bin/echo` on Unix).
pub fn tool_str(name: &str) -> String {
    slash_path(&tool(name))
}

/// [`tool_str`] with a `'static` lifetime, for argv literals that mix it
/// with `&str` (`[sleep_bin(), "5"]`). Resolved once per name and kept for
/// the life of the test process.
pub fn tool_static(name: &str) -> &'static str {
    static CACHE: OnceLock<Mutex<BTreeMap<String, &'static str>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(found) = cache.get(name) {
        return found;
    }
    let leaked: &'static str = Box::leak(tool_str(name).into_boxed_str());
    cache.insert(name.to_owned(), leaked);
    leaked
}

/// The first `name` on this process's `PATH`, as an absolute host path —
/// `git` → `/usr/bin/git` or `C:\Program Files\Git\cmd\git.exe`. On
/// Windows the executable extensions in `PATHEXT` (default `.EXE;.CMD;
/// .BAT;.COM`) are tried the way `CreateProcess` would; `which(1)` from an
/// MSYS userland answers with an MSYS path (`/mingw64/bin/git`) that no
/// Windows API resolves, which is why tests use this instead.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into());
        std::iter::once(String::new())
            .chain(pathext.split(';').map(|ext| ext.trim().to_owned()))
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in &extensions {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Pids of every live process (other than the probe itself) whose command
/// line contains `marker` — the survivor check after a process-tree kill.
/// `pgrep -f` on Unix; `Get-CimInstance Win32_Process` on Windows, where
/// MSYS `pgrep` only sees MSYS processes and would report nothing for a
/// surviving native child.
pub fn processes_mentioning(marker: &str) -> Vec<u32> {
    assert!(
        !marker.is_empty()
            && marker
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "marker is a plain token, got {marker:?}"
    );
    let output = if cfg!(windows) {
        std::process::Command::new(POWERSHELL)
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!(
                "Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -like '*{marker}*' \
-and $_.ProcessId -ne $PID }} | Select-Object -ExpandProperty ProcessId"
            ))
            .output()
    } else {
        // The `[x]` class keeps the probe from matching its own argv, which
        // carries the marker too.
        let (head, tail) = marker.split_at(marker.len() - 1);
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(format!("{head}[{tail}]"))
            .output()
    };
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

/// Whether the process `pid` is alive: `kill -0` on Unix, `tasklist` on
/// Windows.
pub fn process_alive(pid: u32) -> bool {
    if cfg!(windows) {
        let Ok(output) = std::process::Command::new(TASKLIST)
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    } else {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

#[cfg(windows)]
const POWERSHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
#[cfg(not(windows))]
const POWERSHELL: &str = "powershell";
#[cfg(windows)]
const TASKLIST: &str = r"C:\Windows\System32\tasklist.exe";
#[cfg(not(windows))]
const TASKLIST: &str = "tasklist";

/// The POSIX shell for `sh -c` fixtures.
pub fn sh() -> PathBuf {
    tool("sh")
}

/// [`sh`] in the `/`-separated string form.
pub fn sh_str() -> String {
    tool_str("sh")
}

/// A host path rendered with `/` separators — what `sh` on any host reads
/// back as the same file (backslashes are escapes to a POSIX shell).
pub fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A host path quoted for a `sh` script body: `'/tmp/x'`, `'C:/Users/x'`.
/// Single quotes so nothing inside is expanded; a quote in the path itself
/// is spliced as `'\''`.
pub fn sh_quote(path: &Path) -> String {
    let text = slash_path(path);
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Line terminator the fixture `echo` emits on this host. Git for Windows'
/// `echo.exe` writes `\n` like every other POSIX `echo`; this exists so a
/// test states the assumption rather than hard-coding it.
pub const ECHO_NEWLINE: &str = "\n";

fn where_tools_live() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        #[cfg(windows)]
        {
            format!(
                "looked in {} and on PATH; install Git for Windows (its usr/bin ships the POSIX tools)",
                GIT_USR_BIN.join(", ")
            )
        }
        #[cfg(not(windows))]
        {
            format!("looked in {}", UNIX_BIN.join(", "))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_tools_resolve_to_absolute_files() {
        for name in ["echo", "sleep", "cat", "sh", "env", "true", "false"] {
            let path = tool(name);
            assert!(path.is_absolute(), "{name}: {}", path.display());
            assert!(path.is_file(), "{name}: {}", path.display());
            let text = tool_str(name);
            assert!(!text.contains('\\'), "{text}");
        }
    }

    #[test]
    fn the_static_form_is_cached_per_name() {
        let first = tool_static("echo");
        let second = tool_static("echo");
        assert!(std::ptr::eq(first, second));
        assert_eq!(first, tool_str("echo"));
    }

    #[test]
    fn find_on_path_resolves_git_to_an_absolute_file() {
        let git = find_on_path("git").expect("git is on PATH in every CI image");
        assert!(git.is_absolute());
        assert!(git.is_file());
        assert_eq!(find_on_path("rapidlm-no-such-program-on-path"), None);
    }

    #[test]
    fn process_probes_see_a_live_sleeper_and_not_a_reaped_one() {
        let marker = format!("rapidlm-fixture-probe-{}", std::process::id());
        let mut child = std::process::Command::new(sh())
            .arg("-c")
            .arg(format!("{} 30 # {marker}", tool_str("sleep")))
            .spawn()
            .expect("spawn sleeper");
        let pid = child.id();
        assert!(process_alive(pid), "the sleeper is alive");
        let seen = processes_mentioning(&marker);
        assert!(seen.contains(&pid), "probe sees {pid} in {seen:?}");
        child.kill().expect("kill");
        child.wait().expect("reap");
        assert!(!process_alive(pid), "a reaped process is not alive");
    }

    #[test]
    fn a_tool_that_does_not_exist_is_none() {
        assert_eq!(find_tool("rapidlm-no-such-fixture-tool"), None);
    }

    #[test]
    #[should_panic(expected = "bare program name")]
    fn a_path_is_not_a_tool_name() {
        let _ = find_tool("/bin/echo");
    }

    #[test]
    fn sh_quote_uses_forward_slashes_and_single_quotes() {
        assert_eq!(sh_quote(Path::new("/tmp/a b")), "'/tmp/a b'");
        assert_eq!(sh_quote(Path::new(r"C:\Users\r\x")), "'C:/Users/r/x'");
        assert_eq!(sh_quote(Path::new("/tmp/it's")), r"'/tmp/it'\''s'");
    }

    #[test]
    fn the_echo_fixture_really_echoes() {
        let out = std::process::Command::new(tool("echo"))
            .arg("hello")
            .output()
            .expect("run echo");
        assert_eq!(out.stdout, format!("hello{ECHO_NEWLINE}").into_bytes());
    }

    #[test]
    fn the_shell_fixture_runs_a_script() {
        let out = std::process::Command::new(sh())
            .arg("-c")
            .arg("printf '%s' ok")
            .output()
            .expect("run sh");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"ok");
    }
}
