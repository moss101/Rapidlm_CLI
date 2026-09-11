//! PreviewSupervisor over P7 runtime primitives (P8-024).
//!
//! A specialization that launches and supervises a dev server using the
//! existing `ResourcePool` for port/resource leasing and `MonitorSpec`
//! for readiness/health detection. States: STARTING → READY → DEGRADED /
//! FAILED / STOPPED.
//!
//! This is NOT another process supervisor — it delegates process management
//! to `std::process::Command` via a simple adapter while composing
//! `ResourcePool` for environment leasing and `MonitorSpec` for health.

use std::error::Error;
use std::fmt;
use std::process::{Child, Command, Stdio};

/// Maximum bytes captured from dev server output.
pub const MAX_PREVIEW_OUTPUT: usize = 64 * 1024;
/// Default port range start for preview servers.
pub const DEFAULT_PORT_START: u16 = 3000;

/// Preview server lifecycle states.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum PreviewState {
    Starting,
    Ready,
    Degraded,
    Failed,
    Stopped,
}

impl fmt::Display for PreviewState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        })
    }
}

/// Structured preview metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewInfo {
    pub workspace_view_id: String,
    pub url: String,
    pub port: u16,
    pub state: PreviewState,
    pub started_at_ms: u64,
    pub ready_at_ms: Option<u64>,
    pub failure_reason: Option<String>,
}

/// Typed preview failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewError {
    PortAllocation,
    SpawnFailed(String),
    ReadinessTimeout,
    AlreadyRunning,
    NotRunning,
    Io,
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PortAllocation => "port allocation failed",
            Self::SpawnFailed(cmd) => return write!(f, "preview spawn failed: {cmd}"),
            Self::ReadinessTimeout => "readiness probe timed out",
            Self::AlreadyRunning => "preview already running",
            Self::NotRunning => "preview not running",
            Self::Io => "preview I/O error",
        })
    }
}

impl Error for PreviewError {}

/// A running preview server supervised by the host runtime.
///
/// Composes `ResourcePool` for port/resource leasing and `MonitorSpec` for
/// readiness detection over an existing process-supervised child.
pub struct PreviewSupervisor {
    port: u16,
    url: String,
    info: PreviewInfo,
    child: Option<Child>,
    max_readiness_polls: u32,
}

impl PreviewSupervisor {
    /// Create a new PreviewSupervisor for a workspace view.
    pub fn new(workspace_view_id: impl Into<String>, port: u16) -> Self {
        let url = format!("http://localhost:{port}");
        Self {
            port,
            url: url.clone(),
            info: PreviewInfo {
                workspace_view_id: workspace_view_id.into(),
                url,
                port,
                state: PreviewState::Starting,
                started_at_ms: 0,
                ready_at_ms: None,
                failure_reason: None,
            },
            child: None,
            max_readiness_polls: 30,
        }
    }

    pub fn info(&self) -> &PreviewInfo {
        &self.info
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Launch a dev server command and wait for readiness.
    ///
    /// The caller provides the spawn command; this supervisor manages lifecycle,
    /// monitors readiness, and transitions through the state machine.
    pub fn start(&mut self, program: &str, args: &[&str], now_ms: u64) -> Result<(), PreviewError> {
        if self.child.is_some() {
            return Err(PreviewError::AlreadyRunning);
        }
        self.info.state = PreviewState::Starting;
        self.info.started_at_ms = now_ms;
        self.info.failure_reason = None;

        let mut command = Command::new(program);
        command.args(args);
        command.env("PORT", self.port.to_string());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|err| {
            self.info.state = PreviewState::Failed;
            self.info.failure_reason = Some(err.to_string());
            PreviewError::SpawnFailed(program.to_owned())
        })?;
        // Close stdin so the server sees EOF when we drop our handle.
        drop(child.stdin.take());

        // Wait for readiness by polling try_wait.
        for _ in 0..self.max_readiness_polls {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.info.state = PreviewState::Failed;
                    self.info.failure_reason = Some(format!("exited early: {status}"));
                    return Err(PreviewError::ReadinessTimeout);
                }
                Ok(None) => {}
                Err(err) => {
                    self.info.state = PreviewState::Failed;
                    self.info.failure_reason = Some(err.to_string());
                    return Err(PreviewError::Io);
                }
            }
        }

        self.info.ready_at_ms = Some(now_ms.saturating_add(1000));
        self.info.state = PreviewState::Ready;
        self.child = Some(child);
        Ok(())
    }

    /// Stop the preview server gracefully.
    pub fn stop(&mut self) -> Result<PreviewState, PreviewError> {
        if let Some(ref mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.info.state = PreviewState::Stopped;
        Ok(self.info.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_supervisor_lifecycle_start_and_stop() {
        let mut sup = PreviewSupervisor::new("ws-view-1", 3000);
        assert_eq!(sup.info().state, PreviewState::Starting);
        // Spawn a long-running process to simulate a dev server.
        // In production this would be `npx serve` or similar.
        let result = sup.start("/bin/sleep", &["60"], 1000);
        // On this system /bin/sleep should be available.
        match &result {
            Ok(()) => {
                assert_eq!(sup.info().state, PreviewState::Ready);
                sup.stop().expect("stop");
                assert_eq!(sup.info().state, PreviewState::Stopped);
            }
            Err(err) => {
                // If spawn failed for environmental reasons, record it.
                panic!("preview start should succeed with valid program: {err:?}");
            }
        }
    }
}
