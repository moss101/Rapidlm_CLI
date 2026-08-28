//! Real-model tool-calling benchmark (OPT-IN, needs credentials).
//!
//! Drives the actual `rapid` binary against a real OpenRouter model using
//! the user's own config, inside an isolated temp `RAPIDLM_HOME` so the
//! user's trust catalog is untouched. Skipped unless `RAPIDLM_REAL_BENCH=1`
//! is set, so plain `cargo test` stays hermetic.
//!
//! Run: `RAPIDLM_REAL_BENCH=1 cargo test -p rapid --test real_model_bench -- --nocapture`

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kernel::{CancellationToken, ProjectIdentity, ProjectTrustStore, TrustStatus};

const USER_HOME_CONFIG: &str = ".rapidlm/config.toml";
const REAL_BENCH_ENV: &str = "RAPIDLM_REAL_BENCH";
/// Generous per-run ceiling: free-tier models can be slow or queue.
const RUN_DEADLINE: Duration = Duration::from_secs(300);

fn skipped(reason: &str) {
    println!("SKIPPED: {reason}");
}

struct RealProject {
    home: PathBuf,
    project: PathBuf,
}

impl RealProject {
    /// Copy the user's model config into an isolated home and grant the
    /// project trust there.
    fn new(tag: &str) -> Result<Self, String> {
        let user_home = std::env::var("HOME").map_err(|_| "no HOME".to_owned())?;
        let source = Path::new(&user_home).join(USER_HOME_CONFIG);
        let config = std::fs::read_to_string(&source)
            .map_err(|err| format!("read {}: {err}", source.display()))?;
        // With RAPIDLM_HOME set, the CLI resolves config and the trust
        // catalog directly under that directory (no `.rapidlm` component).
        let home = temp_dir(&format!("realbench-{tag}-home"));
        std::fs::create_dir_all(&home).expect("home dir");
        std::fs::write(home.join("config.toml"), &config)
            .map_err(|err| format!("write config: {err}"))?;
        let project = temp_dir(&format!("realbench-{tag}-proj"));
        std::fs::create_dir_all(project.join(".rapidlm")).expect("project marker");
        let root = std::fs::canonicalize(&project).expect("canon");
        let identity = ProjectIdentity::new(root, None).expect("identity");
        let store = ProjectTrustStore::open(home.join("project-trust.json"));
        store
            .set(&identity, TrustStatus::Trusted, &CancellationToken::new())
            .map_err(|err| format!("grant trust: {err:?}"))?;
        Ok(Self { home, project })
    }

    fn seed(&self, name: &str, content: &str) {
        std::fs::write(self.project.join(name), content).expect("seed file");
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.project.join(name)).unwrap_or_default()
    }
}

impl Drop for RealProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

struct RealRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    wall: Duration,
    timed_out: bool,
}

/// Run the real binary with a kill-on-deadline wrapper (free models queue).
fn run_real(project: &Path, home: &Path, prompt: &str, scenario: &str) -> RealRun {
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["exec", prompt])
        .current_dir(project)
        .env("RAPIDLM_HOME", home)
        .env("RAPIDLM_PERMISSION_MODE", "acceptEdits")
        .env_remove("RAPIDLM_CONFIG")
        .env_remove("RAPIDLM_MODEL")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rapid");
    let deadline = started + RUN_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = child.wait_with_output().expect("collect output");
    let wall = started.elapsed();
    let run = RealRun {
        code: status.as_ref().and_then(|status| status.code()),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        wall,
        timed_out: status.is_none(),
    };
    println!(
        "real bench scenario={scenario} wall_ms={} exit={:?} timed_out={}",
        run.wall.as_millis(),
        run.code,
        run.timed_out
    );
    // Persist the run output as evidence when an evidence dir is provided.
    if let Ok(dir) = std::env::var("RAPIDLM_EVIDENCE_DIR") {
        let base = std::path::Path::new(&dir).join(format!("real-{scenario}"));
        let _ = std::fs::write(format!("{}-{}.out.log", base.to_string_lossy(), std::process::id()), &output.stdout);
        let _ = std::fs::write(format!("{}-{}.err.log", base.to_string_lossy(), std::process::id()), &output.stderr);
    }
    run
}

#[test]
fn real_s1_single_tool_call_creates_a_file() {
    if std::env::var(REAL_BENCH_ENV).is_err() {
        return skipped("set RAPIDLM_REAL_BENCH=1 to run against the real model");
    }
    let project = match RealProject::new("s1") {
        Ok(project) => project,
        Err(reason) => return skipped(&reason),
    };
    let run = run_real(
        &project.project,
        &project.home,
        "Create a file named hello.txt whose content is exactly: hello from rapidlm",
        "S1_smoke",
    );
    assert!(!run.timed_out, "run exceeded {}s: {}", RUN_DEADLINE.as_secs(), run.stderr);
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    let content = project.read("hello.txt");
    assert!(
        content.contains("hello from rapidlm"),
        "the tool call must have landed on disk; got: {content:?}"
    );
}

#[test]
fn real_s2_cross_step_memory_two_reads_then_sum() {
    // Sequential reads: the sum value from step 1 must still be usable at
    // the write step, two exchanges later.
    if std::env::var(REAL_BENCH_ENV).is_err() {
        return skipped("set RAPIDLM_REAL_BENCH=1 to run against the real model");
    }
    let project = match RealProject::new("s2") {
        Ok(project) => project,
        Err(reason) => return skipped(&reason),
    };
    project.seed("a.txt", "17\n");
    project.seed("b.txt", "25\n");
    let run = run_real(
        &project.project,
        &project.home,
        "Do this in separate steps: FIRST use repo.read on a.txt only and wait for the \
         result. THEN use repo.read on b.txt. THEN use workspace.write to create sum.txt \
         containing just the sum of the two numbers you read. Finally state the sum.",
        "S2_cross_step",
    );
    assert!(!run.timed_out, "run exceeded {}s: {}", RUN_DEADLINE.as_secs(), run.stderr);
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    let content = project.read("sum.txt");
    assert!(
        content.contains("42"),
        "sum.txt must contain 42 (17+25) — cross-step memory defect if missing: {content:?}"
    );
    assert!(run.stdout.contains("42"), "final answer must state 42: {}", run.stdout);
}

#[test]
fn real_s3_multi_file_rename_with_verification() {
    // Search, edit three files, and verify with a shell command: a realistic
    // heavy agentic task exercising every wired tool.
    if std::env::var(REAL_BENCH_ENV).is_err() {
        return skipped("set RAPIDLM_REAL_BENCH=1 to run against the real model");
    }
    let project = match RealProject::new("s3") {
        Ok(project) => project,
        Err(reason) => return skipped(&reason),
    };
    for name in ["src_a.rs", "src_b.rs", "src_c.rs"] {
        project.seed(name, "fn legacy_fn() {}\n// uses legacy_fn\n");
    }
    project.seed("check.sh", "#!/bin/sh\n! grep -qr legacy_fn . \n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            project.project.join("check.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
    }
    let run = run_real(
        &project.project,
        &project.home,
        "Rename the function legacy_fn to modern_fn in every .rs file in this workspace \
         (including comment mentions). When done, run ./check.sh with shell.exec to verify, \
         then report how many files you changed.",
        "S3_rename",
    );
    assert!(!run.timed_out, "run exceeded {}s: {}", RUN_DEADLINE.as_secs(), run.stderr);
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    for name in ["src_a.rs", "src_b.rs", "src_c.rs"] {
        let content = project.read(name);
        assert!(
            content.contains("modern_fn") && !content.contains("legacy_fn"),
            "{name} must be fully renamed; got: {content:?}"
        );
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-realbench-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}
