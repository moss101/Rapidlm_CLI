# Sandbox and Process Supervisor API

```rust
pub struct ExecSpec {
    pub argv: Vec<OsString>,
    pub cwd: RepoPath,
    pub env: BTreeMap<String, SecretOrValue>,
    pub stdin: StdinSpec,
    pub timeout: Option<Duration>,
    pub output_limit: ByteSize,
    pub sandbox: SandboxSpec,
    pub network: NetworkPolicy,
}

pub struct ExecResult {
    pub exit: ExitStatus,
    pub stdout: OutputRef,
    pub stderr: OutputRef,
    pub duration_ms: u64,
    pub changed_paths: Vec<RepoPath>,
}
```

Commands are executed directly from argv; shell-string execution requires explicit `shell=true` and a higher-risk capability. Process groups/jobs are supervised and killed as a tree. Output exceeding the inline limit is spooled to artifacts. Timeout/cancellation produce explicit terminal status, never an ambiguous generic error.
