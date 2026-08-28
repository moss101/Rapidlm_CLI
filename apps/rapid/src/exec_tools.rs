//! Workspace-scoped tools for the `exec` subcommand: a bounded file write and
//! a bounded file read, gated on project trust. Paths are relative and must
//! resolve strictly inside the trusted workspace root — absolute paths, `..`
//! components, and symlink escapes are refused, and contents are byte-capped.
//! The surface is deliberately minimal: no shell execution, no network.

use std::fs;
use std::path::{Component, Path, PathBuf};

use agent_runtime::{
    CancellationToken, ProposedToolCall, ToolDriver, ToolStepError, ToolStepResult, ToolSurface,
    ValidatedToolCall,
};

use crate::host::NoopTools;

/// Tool name for a bounded workspace file write.
pub const WORKSPACE_WRITE_TOOL: &str = "workspace.write";
/// Tool name for a bounded workspace file read.
pub const WORKSPACE_READ_TOOL: &str = "workspace.read";
/// Hard byte cap on one tool call's JSON arguments.
pub const MAX_TOOL_ARGUMENTS_BYTES: usize = 8 * 1024;
/// Hard byte cap on a relative workspace path.
pub const MAX_TOOL_PATH_BYTES: usize = 512;
/// Hard byte cap on one file write.
pub const MAX_WRITE_BYTES: usize = 64 * 1024;
/// Hard byte cap on one file read returned to the model.
pub const MAX_READ_BYTES: usize = 4 * 1024;

/// Typed workspace-tools setup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolSetupError {
    /// The workspace root does not exist or is not a directory.
    RootNotADirectory,
    /// The workspace root could not be canonicalized.
    RootUnresolvable,
}

impl std::fmt::Display for ToolSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RootNotADirectory => "workspace root is not a directory",
            Self::RootUnresolvable => "workspace root could not be resolved",
        })
    }
}

/// Bounded tools rooted at one canonical workspace directory.
pub struct WorkspaceTools {
    root: PathBuf,
}

impl WorkspaceTools {
    /// Bind the tools to a workspace root. The root is canonicalized once so
    /// every containment check compares against the real directory.
    pub fn open(root: &Path) -> Result<Self, ToolSetupError> {
        if !root.is_dir() {
            return Err(ToolSetupError::RootNotADirectory);
        }
        let root = root.canonicalize().map_err(|_| ToolSetupError::RootUnresolvable)?;
        Ok(Self { root })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a checked relative path inside the root. The containing
    /// directory is created if missing and canonicalized, so a symlinked
    /// directory cannot move the target outside the workspace.
    fn resolve_in_root(&self, relative: &str) -> Result<PathBuf, ToolStepError> {
        let target = self.root.join(checked_relative(relative)?);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
            let resolved = parent.canonicalize().map_err(|_| ToolStepError::Invalid)?;
            if !resolved.starts_with(self.root()) {
                return Err(ToolStepError::Invalid);
            }
        }
        Ok(target)
    }
}

/// Pure relative-path checks shared by validation and execution: non-empty,
/// bounded, no absolute form, no parent/root/prefix components.
fn checked_relative(relative: &str) -> Result<&Path, ToolStepError> {
    if relative.is_empty() || relative.len() > MAX_TOOL_PATH_BYTES {
        return Err(ToolStepError::Invalid);
    }
    let path = Path::new(relative);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolStepError::Invalid);
    }
    Ok(path)
}

struct WriteArgs {
    path: String,
    content: String,
}

struct ReadArgs {
    path: String,
}

/// Parse bounded `{"path": ..., "content": ...}` arguments; unknown keys,
/// wrong types, and out-of-bounds values are refused.
fn parse_write_args(raw: &str) -> Result<WriteArgs, ToolStepError> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if object.len() != 2 {
        return Err(ToolStepError::Invalid);
    }
    let path = object
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    let content = object
        .get("content")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    checked_relative(path)?;
    if content.len() > MAX_WRITE_BYTES {
        return Err(ToolStepError::Invalid);
    }
    Ok(WriteArgs {
        path: path.to_owned(),
        content: content.to_owned(),
    })
}

/// Parse bounded `{"path": ...}` arguments.
fn parse_read_args(raw: &str) -> Result<ReadArgs, ToolStepError> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if object.len() != 1 {
        return Err(ToolStepError::Invalid);
    }
    let path = object
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    Ok(ReadArgs {
        path: checked_relative(path)?.to_string_lossy().into_owned(),
    })
}

/// Largest valid UTF-8 prefix within `MAX_READ_BYTES`, with an explicit
/// truncation marker when content was cut.
fn bounded_text(bytes: &[u8]) -> String {
    let mut end = bytes.len().min(MAX_READ_BYTES);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    let mut summary = String::from_utf8(bytes[..end].to_vec()).unwrap_or_default();
    if bytes.len() > end {
        summary.push_str("\n[truncated]");
    }
    summary
}

impl ToolDriver for WorkspaceTools {
    fn tool_surface(&self) -> Vec<ToolSurface> {
        vec![
            ToolSurface::new(
                WORKSPACE_WRITE_TOOL,
                "Create or overwrite a UTF-8 text file inside the trusted workspace. \
                 Arguments JSON: {\"path\":\"<workspace-relative path>\",\"content\":\"<text>\"}.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "workspace-relative file path"
                        },
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            ),
            ToolSurface::new(
                WORKSPACE_READ_TOOL,
                "Read a UTF-8 text file inside the trusted workspace; the returned \
                 content is bounded. Arguments JSON: {\"path\":\"<workspace-relative path>\"}.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "workspace-relative file path"
                        }
                    },
                    "required": ["path"]
                }),
            ),
        ]
    }

    fn validate(
        &mut self,
        call: &ProposedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError> {
        cancel.check().map_err(|_| ToolStepError::Cancelled)?;
        if call.arguments().len() > MAX_TOOL_ARGUMENTS_BYTES {
            return Err(ToolStepError::Invalid);
        }
        match call.tool() {
            WORKSPACE_WRITE_TOOL => {
                parse_write_args(call.arguments())?;
            }
            WORKSPACE_READ_TOOL => {
                parse_read_args(call.arguments())?;
            }
            _ => return Err(ToolStepError::Invalid),
        }
        Ok(ValidatedToolCall::from_proposed(call))
    }

    fn execute(
        &mut self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        cancel.check().map_err(|_| ToolStepError::Cancelled)?;
        match call.tool() {
            WORKSPACE_WRITE_TOOL => {
                let args = parse_write_args(call.arguments())?;
                let target = self.resolve_in_root(&args.path)?;
                fs::write(&target, args.content.as_bytes()).map_err(|_| ToolStepError::Failed)?;
                Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary: format!("wrote {} bytes to {}", args.content.len(), args.path),
                })
            }
            WORKSPACE_READ_TOOL => {
                let args = parse_read_args(call.arguments())?;
                let target = self.resolve_in_root(&args.path)?;
                match fs::read(&target) {
                    Ok(bytes) => Ok(ToolStepResult::Succeeded {
                        call_id: call.call_id().to_owned(),
                        summary: bounded_text(&bytes),
                    }),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        // Model-visible, handled failure: the turn continues
                        // and the model can correct the path.
                        Ok(ToolStepResult::Failed {
                            call_id: call.call_id().to_owned(),
                            handled: true,
                        })
                    }
                    Err(_) => Err(ToolStepError::Failed),
                }
            }
            _ => Err(ToolStepError::Invalid),
        }
    }
}

/// Tool surface for one exec run: the workspace driver when the project is
/// trusted, otherwise the fail-closed no-op surface that refuses every call.
pub enum ExecTools {
    Noop(NoopTools),
    Workspace(WorkspaceTools),
}

impl ExecTools {
    /// The untrusted surface: every proposed tool call is refused.
    pub fn noop() -> Self {
        Self::Noop(NoopTools)
    }

    /// The trusted workspace surface rooted at `root`.
    pub fn workspace(root: &Path) -> Result<Self, ToolSetupError> {
        Ok(Self::Workspace(WorkspaceTools::open(root)?))
    }
}

impl ToolDriver for ExecTools {
    fn tool_surface(&self) -> Vec<ToolSurface> {
        match self {
            Self::Noop(tools) => tools.tool_surface(),
            Self::Workspace(tools) => tools.tool_surface(),
        }
    }

    fn validate(
        &mut self,
        call: &ProposedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError> {
        match self {
            Self::Noop(tools) => tools.validate(call, cancel),
            Self::Workspace(tools) => tools.validate(call, cancel),
        }
    }

    fn execute(
        &mut self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        match self {
            Self::Noop(tools) => tools.execute(call, cancel),
            Self::Workspace(tools) => tools.execute(call, cancel),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{PreservedLiveContext, run_live_exec};
    use agent_runtime::{
        AgentExecutionRequest, AgentRole, AgentSpec, AgentTerminalStatus, ContextRetryPolicy,
        ModelStepError, ModelStepOutput,
    };
    use protocol::{AgentId, SessionId, WorkspaceViewId};
    use std::collections::VecDeque;

    /// Temp workspace root removed on drop.
    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rapid-exec-tools-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.subsec_nanos())
                    .unwrap_or(0)
            ));
            fs::create_dir_all(&dir).expect("temp root");
            Self(dir)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Scripted live model: proposed tool calls, then a terminal answer.
    struct ScriptedModel {
        outputs: VecDeque<Result<ModelStepOutput, ModelStepError>>,
    }
    impl ScriptedModel {
        fn write_then_answer(path: &str, content: &str, answer: &str) -> Self {
            let call = ProposedToolCall::new(
                "c1",
                WORKSPACE_WRITE_TOOL,
                format!(r#"{{"path":"{path}","content":"{content}"}}"#),
            )
            .expect("call");
            Self {
                outputs: VecDeque::from(vec![
                    Ok(ModelStepOutput::ToolCalls {
                        calls: vec![call],
                        tokens: 1,
                    }),
                    Ok(ModelStepOutput::Terminal {
                        text: answer.to_owned(),
                        tokens: 1,
                    }),
                ]),
            }
        }
    }
    impl crate::host::LiveModelCall for ScriptedModel {
        fn step(
            &mut self,
            _blocks: &[context_engine::compile::ContextBlock],
            _prior_tools: &[ToolStepResult],
            _tool_surface: &[ToolSurface],
            _cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            self.outputs
                .pop_front()
                .unwrap_or(Err(ModelStepError::Failed))
        }
    }

    fn exec_request() -> AgentExecutionRequest {
        let spec = AgentSpec::builder(
            AgentId::new(),
            AgentRole::Coder,
            "create a file",
            WorkspaceViewId::new(),
        )
        .permissions_profile("work")
        .build()
        .expect("spec");
        AgentExecutionRequest::new(spec, SessionId::new())
    }

    fn preserved() -> PreservedLiveContext {
        PreservedLiveContext::new("create a file", Vec::new(), "", "", 8192, 256).expect("preserved")
    }

    #[test]
    fn write_then_read_roundtrip_stays_inside_the_root() {
        let root = TempRoot::new("roundtrip");
        let mut tools = WorkspaceTools::open(&root.0).expect("tools");
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"src/main.rs","content":"fn main() { println!(\"hi\"); }"}"#,
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("execute");
        assert!(matches!(result, ToolStepResult::Succeeded { .. }));
        let written = fs::read(root.0.join("src/main.rs")).expect("file exists");
        assert_eq!(written, b"fn main() { println!(\"hi\"); }");

        let read = ProposedToolCall::new(
            "c2",
            WORKSPACE_READ_TOOL,
            r#"{"path":"src/main.rs"}"#,
        )
        .expect("call");
        let validated = tools.validate(&read, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("execute");
        match result {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("fn main()"));
            }
            other => panic!("expected read success, got {other:?}"),
        }
    }

    #[test]
    fn traversal_absolute_and_oversize_arguments_are_refused() {
        let root = TempRoot::new("refuse");
        let mut tools = WorkspaceTools::open(&root.0).expect("tools");
        let cancel = CancellationToken::new();
        for arguments in [
            r#"{"path":"../escape.txt","content":"x"}"#,
            r#"{"path":"/etc/passwd","content":"x"}"#,
            r#"{"path":"a/../../out.txt","content":"x"}"#,
            r#"{"path":"","content":"x"}"#,
            r#"{"path":"ok.txt","content":"x","extra":1}"#,
            r#"{"path":42,"content":"x"}"#,
            // Sized past the tool layer's argument bound but under the turn
            // layer's own 16 KiB cap, so this refusal is ours.
            format!(
                r#"{{"path":"big.txt","content":"{}"}}"#,
                "x".repeat(MAX_TOOL_ARGUMENTS_BYTES + 1)
            )
            .as_str(),
        ] {
            let call =
                ProposedToolCall::new("c1", WORKSPACE_WRITE_TOOL, arguments).expect("call");
            assert!(
                tools.validate(&call, &cancel).is_err(),
                "arguments must be refused: {arguments}"
            );
        }
        // Nothing was written anywhere.
        assert_eq!(fs::read_dir(&root.0).expect("root").count(), 0);
    }

    #[test]
    fn unknown_tools_are_refused() {
        let root = TempRoot::new("unknown");
        let mut tools = WorkspaceTools::open(&root.0).expect("tools");
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new("c1", "shell.exec", "{}").expect("call");
        assert!(matches!(
            tools.validate(&call, &cancel),
            Err(ToolStepError::Invalid)
        ));
    }

    #[test]
    fn missing_read_is_a_handled_model_visible_failure() {
        let root = TempRoot::new("missing");
        let mut tools = WorkspaceTools::open(&root.0).expect("tools");
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new("c1", WORKSPACE_READ_TOOL, r#"{"path":"nope.txt"}"#)
            .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("handled");
        match result {
            ToolStepResult::Failed { handled, .. } => assert!(handled),
            other => panic!("expected handled failure, got {other:?}"),
        }
    }

    #[test]
    fn oversized_reads_are_truncated_with_a_marker() {
        assert_eq!(bounded_text(b"hello"), "hello");
        let big = vec![b'a'; MAX_READ_BYTES + 10];
        let text = bounded_text(&big);
        assert_eq!(text.len(), MAX_READ_BYTES + "\n[truncated]".len());
        assert!(text.ends_with("[truncated]"));
        // Invalid UTF-8 tails are cut on a char boundary, never panicked.
        let mut mixed = vec![b'x'; 4];
        mixed.extend_from_slice(&[0xf0, 0x9f, 0x92]); // cut emoji
        assert!(!bounded_text(&mixed).contains('\u{fffd}'));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused() {
        let root = TempRoot::new("symlink");
        let outside = TempRoot::new("symlink-outside");
        std::os::unix::fs::symlink(&outside.0, root.0.join("link")).expect("symlink");
        let mut tools = WorkspaceTools::open(&root.0).expect("tools");
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"link/escape.txt","content":"x"}"#,
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("shape is valid");
        assert!(
            tools.execute(&validated, &cancel).is_err(),
            "a symlinked directory must not move writes outside the root"
        );
        assert!(!outside.0.join("escape.txt").exists(), "nothing escaped");
    }

    #[test]
    fn trusted_workspace_tools_complete_a_write_task_end_to_end() {
        // Drives the real exec entry (run_live_exec) with a scripted model
        // that proposes a file write and the real workspace tool driver.
        let root = TempRoot::new("e2e");
        let mut tools = ExecTools::workspace(&root.0).expect("tools");
        let mut events = Vec::new();
        let model = ScriptedModel::write_then_answer("notes/plan.md", "do the thing", "wrote it");
        let outcome = run_live_exec(
            preserved(),
            model,
            &exec_request(),
            &mut tools,
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.status(), AgentTerminalStatus::Succeeded);
        assert_eq!(outcome.result.summary(), "wrote it");
        assert_eq!(
            fs::read_to_string(root.0.join("notes/plan.md")).expect("file"),
            "do the thing"
        );
    }

    #[test]
    fn untrusted_surface_refuses_the_write_and_fails_closed() {
        let root = TempRoot::new("untrusted");
        let mut tools = ExecTools::noop();
        let mut events = Vec::new();
        let model = ScriptedModel::write_then_answer("plan.md", "do the thing", "unreachable");
        let outcome = run_live_exec(
            preserved(),
            model,
            &exec_request(),
            &mut tools,
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("typed failed turn, not an execution error");
        assert_eq!(outcome.result.status(), AgentTerminalStatus::Failed);
        assert_eq!(
            outcome.failure_cause,
            None,
            "a tool refusal is not a provider failure"
        );
        assert!(!root.0.join("plan.md").exists(), "fail-closed: nothing written");
    }
}
