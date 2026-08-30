//! `--json-schema`: a constrained structured-output mode for `rapid exec`.
//!
//! Wraps an existing `ToolDriver` with one synthetic tool whose `parameters`
//! is exactly the caller-supplied JSON Schema. The model is expected to call
//! it once with the final result; the call's arguments are validated against
//! the schema before being accepted, so a malformed result is a model-
//! correctable tool failure, never a silent pass-through. Every other tool
//! call is delegated to the wrapped driver unchanged — this only adds a tool,
//! it never removes or reinterprets one.

use std::cell::RefCell;
use std::rc::Rc;

use agent_runtime::{
    CancellationToken, ProposedToolCall, ToolDriver, ToolKind, ToolStepError, ToolStepExchange,
    ToolStepResult, ToolSurface, ValidatedToolCall,
};

/// Fixed synthetic tool name. Chosen to be unambiguous in transcripts and
/// unlikely to collide with a real workspace tool.
const TOOL_NAME: &str = "emit_structured_result";

/// Typed failure building the wrapper (an invalid `--json-schema` document).
/// Display never echoes the schema itself — it can be arbitrarily large.
#[derive(Debug)]
pub struct InvalidSchema(String);

impl std::fmt::Display for InvalidSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid --json-schema document: {}", self.0)
    }
}

impl std::error::Error for InvalidSchema {}

pub struct StructuredOutputTools<T> {
    inner: T,
    schema: serde_json::Value,
    validator: jsonschema::Validator,
    /// The most recent validated call's arguments, verbatim. `None` until
    /// the model calls the synthetic tool with a schema-conformant result.
    captured: Rc<RefCell<Option<String>>>,
}

impl<T> StructuredOutputTools<T> {
    pub fn new(inner: T, schema: serde_json::Value) -> Result<Self, InvalidSchema> {
        let validator =
            jsonschema::Validator::new(&schema).map_err(|err| InvalidSchema(err.to_string()))?;
        Ok(Self {
            inner,
            schema,
            validator,
            captured: Rc::new(RefCell::new(None)),
        })
    }

    /// Shared handle to the captured result, readable after the turn ends
    /// regardless of how many times this wrapper is subsequently borrowed.
    pub fn captured_result(&self) -> Rc<RefCell<Option<String>>> {
        self.captured.clone()
    }
}

impl<T: ToolDriver> ToolDriver for StructuredOutputTools<T> {
    fn validate(
        &mut self,
        call: &ProposedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError> {
        if call.tool() == TOOL_NAME {
            return Ok(ValidatedToolCall::from_proposed(call));
        }
        self.inner.validate(call, cancel)
    }

    fn execute(
        &mut self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        if call.tool() != TOOL_NAME {
            return self.inner.execute(call, cancel);
        }
        let parsed: serde_json::Value = match serde_json::from_str(call.arguments()) {
            Ok(value) => value,
            Err(err) => {
                return Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(format!("arguments were not valid JSON: {err}")),
                });
            }
        };
        if let Err(err) = self.validator.validate(&parsed) {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(format!("result does not match the required schema: {err}")),
            });
        }
        *self.captured.borrow_mut() = Some(parsed.to_string());
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary: "structured result accepted".to_owned(),
        })
    }

    fn tool_surface(&self) -> Vec<ToolSurface> {
        let mut surface = self.inner.tool_surface();
        surface.push(ToolSurface::new(
            TOOL_NAME,
            "Call this exactly once with the final result of the task, as JSON matching the \
             required schema exactly. This is the only way to report a structured result.",
            self.schema.clone(),
        ));
        surface
    }

    fn tool_kind(&self, tool: &str) -> ToolKind {
        if tool == TOOL_NAME {
            ToolKind::Read
        } else {
            self.inner.tool_kind(tool)
        }
    }

    fn drain_notifications(&mut self) -> Vec<ToolStepExchange> {
        self.inner.drain_notifications()
    }

    fn execute_batch(
        &mut self,
        calls: &[ValidatedToolCall],
        cancel: &CancellationToken,
    ) -> Vec<Result<ToolStepResult, ToolStepError>> {
        // The synthetic tool is handled inline (schema validation is cheap
        // and synchronous); every other call keeps the inner driver's own
        // batch dispatch (e.g. real concurrency for independent reads), not
        // the trait's sequential default.
        let mut results: Vec<Option<Result<ToolStepResult, ToolStepError>>> =
            (0..calls.len()).map(|_| None).collect();
        let mut inner_calls = Vec::new();
        let mut inner_indices = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            if call.tool() == TOOL_NAME {
                results[i] = Some(self.execute(call, cancel));
            } else {
                inner_indices.push(i);
                inner_calls.push(call.clone());
            }
        }
        if !inner_calls.is_empty() {
            for (idx, result) in inner_indices
                .into_iter()
                .zip(self.inner.execute_batch(&inner_calls, cancel))
            {
                results[idx] = Some(result);
            }
        }
        results
            .into_iter()
            .map(|r| r.expect("every call index was filled by either branch above"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingDriver {
        surface: Vec<ToolSurface>,
        executed: Vec<String>,
    }

    impl ToolDriver for RecordingDriver {
        fn validate(
            &mut self,
            call: &ProposedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<ValidatedToolCall, ToolStepError> {
            Ok(ValidatedToolCall::from_proposed(call))
        }

        fn execute(
            &mut self,
            call: &ValidatedToolCall,
            _cancel: &CancellationToken,
        ) -> Result<ToolStepResult, ToolStepError> {
            self.executed.push(call.tool().to_owned());
            Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: "inner ran".to_owned(),
            })
        }

        fn tool_surface(&self) -> Vec<ToolSurface> {
            self.surface.clone()
        }
    }

    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false,
        })
    }

    #[test]
    fn tool_surface_adds_the_synthetic_tool_alongside_the_inner_surface() {
        let inner = RecordingDriver {
            surface: vec![ToolSurface::new("read_file", "read", serde_json::json!({}))],
            executed: Vec::new(),
        };
        let wrapped = StructuredOutputTools::new(inner, schema()).expect("valid schema");
        let names: Vec<_> = wrapped.tool_surface().iter().map(ToolSurface::name).map(str::to_owned).collect();
        assert_eq!(names, vec!["read_file", TOOL_NAME]);
    }

    #[test]
    fn valid_result_is_captured_and_reported_succeeded() {
        let inner = RecordingDriver { surface: Vec::new(), executed: Vec::new() };
        let mut wrapped = StructuredOutputTools::new(inner, schema()).expect("valid schema");
        let captured = wrapped.captured_result();
        let cancel = CancellationToken::new();
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("c1", TOOL_NAME, r#"{"answer":"42"}"#).expect("call"),
        );
        let result = wrapped.execute(&call, &cancel).expect("execute");
        assert!(matches!(result, ToolStepResult::Succeeded { .. }));
        assert_eq!(captured.borrow().as_deref(), Some(r#"{"answer":"42"}"#));
    }

    #[test]
    fn schema_mismatch_is_a_handled_failure_not_a_silent_pass() {
        let inner = RecordingDriver { surface: Vec::new(), executed: Vec::new() };
        let mut wrapped = StructuredOutputTools::new(inner, schema()).expect("valid schema");
        let captured = wrapped.captured_result();
        let cancel = CancellationToken::new();
        // Missing the required "answer" field.
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("c1", TOOL_NAME, r#"{"wrong":"field"}"#).expect("call"),
        );
        let result = wrapped.execute(&call, &cancel).expect("execute");
        match result {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("schema"));
            }
            other => panic!("expected a handled failure, got {other:?}"),
        }
        assert!(captured.borrow().is_none());
    }

    #[test]
    fn malformed_json_arguments_are_a_handled_failure() {
        let inner = RecordingDriver { surface: Vec::new(), executed: Vec::new() };
        let mut wrapped = StructuredOutputTools::new(inner, schema()).expect("valid schema");
        let cancel = CancellationToken::new();
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("c1", TOOL_NAME, "not json").expect("call"),
        );
        let result = wrapped.execute(&call, &cancel).expect("execute");
        assert!(matches!(result, ToolStepResult::Failed { handled: true, .. }));
    }

    #[test]
    fn other_tool_calls_delegate_to_the_inner_driver_unchanged() {
        let inner = RecordingDriver { surface: Vec::new(), executed: Vec::new() };
        let mut wrapped = StructuredOutputTools::new(inner, schema()).expect("valid schema");
        let cancel = CancellationToken::new();
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("c1", "read_file", "{}").expect("call"),
        );
        let result = wrapped.execute(&call, &cancel).expect("execute");
        assert!(matches!(result, ToolStepResult::Succeeded { .. }));
        assert_eq!(wrapped.inner.executed, vec!["read_file".to_owned()]);
        assert!(wrapped.captured_result().borrow().is_none());
    }

    #[test]
    fn execute_batch_handles_the_synthetic_tool_inline_and_delegates_the_rest() {
        let inner = RecordingDriver { surface: Vec::new(), executed: Vec::new() };
        let mut wrapped = StructuredOutputTools::new(inner, schema()).expect("valid schema");
        let cancel = CancellationToken::new();
        let calls = vec![
            ValidatedToolCall::from_proposed(
                &ProposedToolCall::new("c1", "read_file", "{}").expect("call"),
            ),
            ValidatedToolCall::from_proposed(
                &ProposedToolCall::new("c2", TOOL_NAME, r#"{"answer":"ok"}"#).expect("call"),
            ),
        ];
        let results = wrapped.execute_batch(&calls, &cancel);
        assert_eq!(results.len(), 2);
        assert!(matches!(results[0].as_ref().unwrap(), ToolStepResult::Succeeded { .. }));
        assert!(matches!(results[1].as_ref().unwrap(), ToolStepResult::Succeeded { .. }));
        assert_eq!(wrapped.inner.executed, vec!["read_file".to_owned()]);
        assert_eq!(wrapped.captured_result().borrow().as_deref(), Some(r#"{"answer":"ok"}"#));
    }

    #[test]
    fn invalid_schema_document_is_rejected_at_construction() {
        let inner = RecordingDriver { surface: Vec::new(), executed: Vec::new() };
        match StructuredOutputTools::new(inner, serde_json::json!({"type": "not-a-type"})) {
            Err(err) => assert!(err.to_string().contains("invalid --json-schema")),
            Ok(_) => panic!("malformed schema must be rejected"),
        }
    }
}
