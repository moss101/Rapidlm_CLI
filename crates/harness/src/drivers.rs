//! P11-003/004/005: ScriptedModel + ReplayProvider on the production
//! `agent_runtime::turn::ModelDriver` seam, and the experiment-pinned
//! LiveProvider that refuses to run without explicit live configuration.

use agent_runtime::turn::{ModelDriver, ModelStepError, ModelStepInput, ModelStepOutput};

/// Deterministic scripted model: replays queued outputs in order.
#[derive(Default)]
pub struct ScriptedModel {
    steps: Vec<ScriptedStep>,
}

/// One scripted model step.
#[derive(Clone, Debug, PartialEq)]
pub enum ScriptedStep {
    Terminal { text: String, tokens: u64 },
    Fail,
}

impl ScriptedModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_terminal(mut self, text: impl Into<String>, tokens: u64) -> Self {
        self.steps.push(ScriptedStep::Terminal {
            text: text.into(),
            tokens,
        });
        self
    }

    pub fn push_failure(mut self) -> Self {
        self.steps.push(ScriptedStep::Fail);
        self
    }

    fn next_output(&mut self) -> Result<ModelStepOutput, ModelStepError> {
        match self.steps.first() {
            Some(ScriptedStep::Terminal { text, tokens }) => {
                let out = ModelStepOutput::Terminal {
                    text: text.clone(),
                    tokens: *tokens,
                    cost_usd_micros: None,
                };
                self.steps.remove(0);
                Ok(out)
            }
            Some(ScriptedStep::Fail) => {
                self.steps.remove(0);
                Err(ModelStepError::Failed)
            }
            None => Err(ModelStepError::BoundExceeded),
        }
    }
}

impl ModelDriver for ScriptedModel {
    fn step(
        &mut self,
        _input: &ModelStepInput<'_>,
        cancel: &agent_runtime::CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        if cancel.is_cancelled() {
            return Err(ModelStepError::Cancelled);
        }
        self.next_output()
    }
}

/// Replay provider: deterministic outputs recorded from a prior run
/// (JSON lines of `{"text":..,"tokens":..}`), also on the ModelDriver seam.
pub struct ReplayProvider {
    inner: ScriptedModel,
}

impl ReplayProvider {
    /// Build from newline-delimited JSON records; malformed lines fail closed
    /// at construction so a replay never diverges mid-run.
    pub fn from_records(jsonl: &str) -> Result<Self, ModelStepError> {
        let mut inner = ScriptedModel::new();
        for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
            let value: serde_json::Value =
                serde_json::from_str(line).map_err(|_| ModelStepError::Failed)?;
            let text = value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or(ModelStepError::Failed)?;
            let tokens = value
                .get("tokens")
                .and_then(serde_json::Value::as_u64)
                .ok_or(ModelStepError::Failed)?;
            inner = inner.push_terminal(text, tokens);
        }
        if jsonl.trim().is_empty() {
            return Err(ModelStepError::Failed);
        }
        Ok(Self { inner })
    }
}

impl ModelDriver for ReplayProvider {
    fn step(
        &mut self,
        input: &ModelStepInput<'_>,
        cancel: &agent_runtime::CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        self.inner.step(input, cancel)
    }
}

/// Live-model experiment pinning. Without an explicitly pinned live endpoint
/// this driver fails closed — the harness never makes live calls implicitly.
pub struct LiveProvider {
    pinned: Option<()>,
}

impl LiveProvider {
    pub fn unpinned() -> Self {
        Self { pinned: None }
    }

    /// Pin to an explicit experiment config. Returns None unless the config
    /// names a live endpoint AND carries the experiment id (P11-021).
    pub fn pin(experiment_id: &str, endpoint_set: bool) -> Option<Self> {
        if endpoint_set && !experiment_id.is_empty() {
            Some(Self { pinned: Some(()) })
        } else {
            None
        }
    }
}

impl ModelDriver for LiveProvider {
    fn step(
        &mut self,
        _input: &ModelStepInput<'_>,
        _cancel: &agent_runtime::CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        // No live network access is claimed or attempted by the harness here;
        // a pinned live run requires the external judge/provider integration.
        match self.pinned {
            Some(()) => Err(ModelStepError::Failed),
            None => Err(ModelStepError::Failed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime::CancellationToken;

    #[test]
    fn scripted_model_replays_terminal_outputs_on_modeldriver_seam() {
        let mut model = ScriptedModel::new()
            .push_terminal("first", 5)
            .push_terminal("second", 7);
        let cancel = CancellationToken::new();
        let input = ModelStepInput::without_tools(1);
        let out = model.step(&input, &cancel).expect("step 1");
        assert_eq!(
            out,
            ModelStepOutput::Terminal {
                text: "first".into(),
                tokens: 5,
                cost_usd_micros: None,
            }
        );
        let out = model.step(&input, &cancel).expect("step 2");
        assert_eq!(
            out,
            ModelStepOutput::Terminal {
                text: "second".into(),
                tokens: 7,
                cost_usd_micros: None,
            }
        );
        // Exhausted script fails closed with BoundExceeded.
        assert_eq!(
            model.step(&input, &cancel),
            Err(ModelStepError::BoundExceeded)
        );
    }

    #[test]
    fn scripted_failure_step_maps_to_failed_and_cancel_is_honored() {
        let mut model = ScriptedModel::new().push_failure();
        let input = ModelStepInput::without_tools(0);
        assert_eq!(
            model.step(&input, &CancellationToken::new()),
            Err(ModelStepError::Failed)
        );
        let mut cancelled_model = ScriptedModel::new().push_terminal("x", 1);
        assert_eq!(
            cancelled_model.step(&input, &{
                let t = CancellationToken::new();
                t.cancel();
                t
            }),
            Err(ModelStepError::Cancelled)
        );
    }

    #[test]
    fn replay_provider_rebuilds_script_from_records_and_fails_closed_on_garbage() {
        let provider = ReplayProvider::from_records(
            "{\"text\":\"a\",\"tokens\":2}\n{\"text\":\"b\",\"tokens\":3}\n",
        )
        .expect("records");
        let mut provider = provider;
        let input = ModelStepInput::without_tools(0);
        let cancel = CancellationToken::new();
        assert!(matches!(
            provider.step(&input, &cancel),
            Ok(ModelStepOutput::Terminal { .. })
        ));
        assert!(ReplayProvider::from_records("{not json").is_err());
        assert!(ReplayProvider::from_records("   \n").is_err());
        assert!(
            ReplayProvider::from_records("{\"text\":\"a\"}").is_err(),
            "missing tokens"
        );
    }

    #[test]
    fn live_provider_requires_experiment_pinning_and_never_calls_out_here() {
        assert!(LiveProvider::pin("", true).is_none());
        assert!(LiveProvider::pin("exp-1", false).is_none());
        let mut pinned = LiveProvider::pin("exp-1", true).expect("pinned");
        let input = ModelStepInput::without_tools(0);
        // Pinned live runs require the external provider integration; the
        // harness seam itself never performs live I/O.
        assert_eq!(
            pinned.step(&input, &CancellationToken::new()),
            Err(ModelStepError::Failed)
        );
    }
}
