//! Bounded, versioned question forms and validated responses.
//!
//! Validation is deliberately separate from resolving a durable wait: an
//! adapter must validate before it consumes that wait, and must not submit a
//! partial response as a completed answer.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_WIRE_BYTES: usize = 8 * 1024;
const MAX_QUESTIONS: usize = 4;
const MAX_OPTIONS: usize = 4;
const MAX_ID_BYTES: usize = 64;
const MAX_QUESTION_BYTES: usize = 1024;
const MAX_LABEL_BYTES: usize = 256;
const MAX_DESCRIPTION_BYTES: usize = 512;
const MAX_OTHER_BYTES: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Form {
    pub version: u8,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<OptionItem>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default = "allow_other_by_default")]
    pub allow_other: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionItem {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Partial,
    Complete,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u8,
    pub status: ResponseStatus,
    pub answers: Vec<Answer>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub question_id: String,
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

fn allow_other_by_default() -> bool {
    true
}

fn text_is_valid(value: &str, max: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max
        && !value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
}

fn id_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

impl Form {
    /// Translate this form into ACP's flat form schema. The transport must
    /// negotiate form elicitation and apply its policy before sending it.
    pub fn elicitation_schema(
        &self,
        progress: Option<&Response>,
    ) -> Result<serde_json::Value, &'static str> {
        self.validate()?;
        if let Some(progress) = progress {
            let raw = serde_json::to_string(progress).map_err(|_| "invalid progress")?;
            if self.parse_response(&raw)?.status != ResponseStatus::Partial {
                return Err("invalid progress status");
            }
        }
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for (index, question) in self.questions.iter().enumerate() {
            let choice_key = format!("q{index}_choice");
            let other_key = format!("q{index}_other");
            let choices: Vec<_> = question
                .options
                .iter()
                .map(|option| {
                    let mut choice = serde_json::json!({"const": option.id, "title": option.label});
                    if let Some(description) = &option.description {
                        choice["description"] = serde_json::Value::String(description.clone());
                    }
                    choice
                })
                .collect();
            let previous = progress.and_then(|p| {
                p.answers
                    .iter()
                    .find(|answer| answer.question_id == question.id)
            });
            let mut schema = if question.multi_select {
                serde_json::json!({"type":"array", "title": question.question, "items":{"anyOf":choices}, "maxItems":question.options.len()})
            } else {
                serde_json::json!({"type":"string", "title": question.question, "oneOf":choices})
            };
            if let Some(answer) = previous {
                if question.multi_select && !answer.selected.is_empty() {
                    schema["default"] = serde_json::json!(answer.selected);
                } else if !question.multi_select
                    && let Some(selected) = answer.selected.first()
                {
                    schema["default"] = serde_json::json!(selected);
                }
            }
            properties.insert(choice_key.clone(), schema);
            if question.allow_other {
                let mut other = serde_json::json!({"type":"string", "title":format!("{} — Other", question.question), "maxLength":MAX_OTHER_BYTES});
                if let Some(text) = previous.and_then(|answer| answer.other.as_ref()) {
                    other["default"] = serde_json::json!(text);
                }
                properties.insert(other_key, other);
            } else {
                required.push(choice_key);
            }
        }
        Ok(serde_json::json!({"type":"object", "properties":properties, "required":required}))
    }

    /// Decode ACP's flat response into the durable form response, rejecting
    /// missing, unknown, duplicated or conflicting choices.
    pub fn parse_elicitation_content(
        &self,
        content: &serde_json::Value,
    ) -> Result<Response, &'static str> {
        self.validate()?;
        if serde_json::to_vec(content)
            .map_err(|_| "invalid elicitation content")?
            .len()
            > MAX_WIRE_BYTES
        {
            return Err("elicitation content exceeds the byte limit");
        }
        let object = content
            .as_object()
            .ok_or("elicitation content must be an object")?;
        let mut allowed = HashSet::new();
        let mut answers = Vec::with_capacity(self.questions.len());
        for (index, question) in self.questions.iter().enumerate() {
            let choice_key = format!("q{index}_choice");
            let other_key = format!("q{index}_other");
            allowed.insert(choice_key.clone());
            if question.allow_other {
                allowed.insert(other_key.clone());
            }
            let selected = match object.get(&choice_key) {
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(serde_json::Value::String(value)) if !question.multi_select => {
                    vec![value.clone()]
                }
                Some(serde_json::Value::Array(values)) if question.multi_select => values
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned).ok_or("invalid selection"))
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err("invalid selection"),
            };
            let other = match object.get(&other_key) {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(value)) if value.trim().is_empty() => None,
                Some(serde_json::Value::String(value)) => Some(value.clone()),
                _ => return Err("invalid custom answer"),
            };
            answers.push(Answer {
                question_id: question.id.clone(),
                selected,
                other,
            });
        }
        if object.keys().any(|key| !allowed.contains(key)) {
            return Err("unknown elicitation field");
        }
        let response = Response {
            version: 1,
            status: ResponseStatus::Complete,
            answers,
        };
        let encoded = serde_json::to_string(&response).map_err(|_| "invalid response")?;
        self.parse_response(&encoded)
    }

    /// Parse an untrusted form without accepting unsupported versions or
    /// ambiguous question/option identities.
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        if raw.len() > MAX_WIRE_BYTES {
            return Err("form exceeds the byte limit");
        }
        let form: Self = serde_json::from_str(raw).map_err(|_| "invalid form JSON")?;
        form.validate()?;
        Ok(form)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 {
            return Err("unsupported form version");
        }
        if self.questions.is_empty() || self.questions.len() > MAX_QUESTIONS {
            return Err("form must contain one to four questions");
        }
        let mut questions = HashSet::new();
        for question in &self.questions {
            if !id_is_valid(&question.id) || !questions.insert(&question.id) {
                return Err("invalid or duplicate question ID");
            }
            if !text_is_valid(&question.question, MAX_QUESTION_BYTES)
                || question.header.as_ref().is_some_and(|h| {
                    !text_is_valid(h, 48)
                        || h.chars().count() > 12
                        || h.chars().any(|c| matches!(c, '\n' | '\t'))
                })
            {
                return Err("invalid question or header");
            }
            if !(2..=MAX_OPTIONS).contains(&question.options.len()) {
                return Err("question must contain two to four options");
            }
            let mut options = HashSet::new();
            for option in &question.options {
                if !id_is_valid(&option.id)
                    || !options.insert(&option.id)
                    || !text_is_valid(&option.label, MAX_LABEL_BYTES)
                    || option
                        .description
                        .as_ref()
                        .is_some_and(|d| !text_is_valid(d, MAX_DESCRIPTION_BYTES))
                {
                    return Err("invalid or duplicate option");
                }
            }
        }
        Ok(())
    }

    /// Validate identities, choices, completeness and cancellation before a
    /// caller changes the durable wait's state.
    pub fn parse_response(&self, raw: &str) -> Result<Response, &'static str> {
        self.validate()?;
        if raw.len() > MAX_WIRE_BYTES {
            return Err("response exceeds the byte limit");
        }
        let response: Response = serde_json::from_str(raw).map_err(|_| "invalid response JSON")?;
        if response.version != 1 {
            return Err("unsupported response version");
        }
        if response.answers.len() > self.questions.len() {
            return Err("too many answers");
        }
        if response.status == ResponseStatus::Cancelled && !response.answers.is_empty() {
            return Err("cancelled response cannot contain answers");
        }
        let mut answered = HashSet::new();
        for answer in &response.answers {
            if !answered.insert(&answer.question_id) {
                return Err("duplicate answer");
            }
            let question = self
                .questions
                .iter()
                .find(|q| q.id == answer.question_id)
                .ok_or("unknown question ID")?;
            let mut selected = HashSet::new();
            for id in &answer.selected {
                if !selected.insert(id) || !question.options.iter().any(|option| option.id == *id) {
                    return Err("unknown or duplicate option ID");
                }
            }
            if answer.other.as_ref().is_some_and(|other| {
                !question.allow_other || !text_is_valid(other, MAX_OTHER_BYTES)
            }) {
                return Err("invalid custom answer");
            }
            let count = answer.selected.len() + usize::from(answer.other.is_some());
            if count == 0 || (!question.multi_select && count != 1) {
                return Err("invalid number of choices");
            }
        }
        if response.status == ResponseStatus::Complete && answered.len() != self.questions.len() {
            return Err("completed response is missing answers");
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORM: &str = r#"{"version":1,"questions":[{"id":"q1","question":"Which?","options":[{"id":"a","label":"A"},{"id":"b","label":"B"}],"multi_select":true},{"id":"q2","question":"Where?","options":[{"id":"x","label":"X"},{"id":"y","label":"Y"}],"allow_other":false}]}"#;

    #[test]
    fn acp_schema_and_response_preserve_choices_other_and_saved_progress() {
        let form = Form::parse(FORM).unwrap();
        let progress = form.parse_response(r#"{"version":1,"status":"partial","answers":[{"question_id":"q1","selected":["a","b"],"other":"custom"}]}"#).unwrap();
        let schema = form.elicitation_schema(Some(&progress)).unwrap();
        assert_eq!(
            schema["properties"]["q0_choice"]["default"],
            serde_json::json!(["a", "b"])
        );
        assert_eq!(schema["properties"]["q0_other"]["default"], "custom");
        assert_eq!(schema["properties"]["q1_choice"]["oneOf"][0]["const"], "x");
        assert_eq!(schema["required"], serde_json::json!(["q1_choice"]));
        let response = form
            .parse_elicitation_content(&serde_json::json!({
                "q0_choice":["a","b"], "q0_other":"custom", "q1_choice":"x"
            }))
            .unwrap();
        assert_eq!(response.status, ResponseStatus::Complete);
        assert_eq!(response.answers[0].selected, vec!["a", "b"]);
        assert_eq!(response.answers[0].other.as_deref(), Some("custom"));
        assert!(
            form.parse_elicitation_content(
                &serde_json::json!({"q0_choice":["a","a"], "q1_choice":"x"})
            )
            .is_err()
        );
        assert!(
            form.parse_elicitation_content(
                &serde_json::json!({"q0_choice":["a"], "q1_choice":"unknown"})
            )
            .is_err()
        );
        assert!(
            form.parse_elicitation_content(
                &serde_json::json!({"q0_choice":["a"], "q1_choice":"x", "extra":"bad"})
            )
            .is_err()
        );
        assert!(
            form.parse_elicitation_content(
                &serde_json::json!({"q0_choice":["a"], "q1_choice":"x", "q1_other":"bad"})
            )
            .is_err()
        );
        assert!(
            form.parse_elicitation_content(
                &serde_json::json!({"q0_other":"x".repeat(MAX_WIRE_BYTES), "q1_choice":"x"})
            )
            .is_err()
        );
    }

    #[test]
    fn multi_question_responses_validate_selection_completeness_and_cancellation() {
        let form = Form::parse(FORM).unwrap();
        let partial = r#"{"version":1,"status":"partial","answers":[{"question_id":"q1","selected":["a","b"],"other":"custom"}]}"#;
        assert_eq!(
            form.parse_response(partial).unwrap().status,
            ResponseStatus::Partial
        );
        let complete = r#"{"version":1,"status":"complete","answers":[{"question_id":"q1","selected":["a","b"]},{"question_id":"q2","selected":["x"]}]}"#;
        assert_eq!(
            form.parse_response(complete).unwrap().status,
            ResponseStatus::Complete
        );
        assert!(
            form.parse_response(&partial.replace("partial", "complete"))
                .is_err()
        );
        assert!(
            form.parse_response(r#"{"version":1,"status":"cancelled","answers":[]}"#)
                .is_ok()
        );
        assert!(
            form.parse_response(&partial.replace("partial", "cancelled"))
                .is_err()
        );
        assert!(
            form.parse_response(&complete.replace("[\"x\"]", "[\"x\",\"y\"]"))
                .is_err()
        );
        assert!(
            form.parse_response(&complete.replace("[\"a\",\"b\"]", "[\"a\",\"a\"]"))
                .is_err()
        );
        assert!(
            form.parse_response(&complete.replace("\"x\"", "\"z\""))
                .is_err()
        );
        assert!(
            form.parse_response(&complete.replace("\"selected\":[\"x\"]", "\"other\":\"outside\""))
                .is_err()
        );
        assert!(
            form.parse_response(
                &complete.replace("\"question_id\":\"q2\"", "\"question_id\":\"q1\"")
            )
            .is_err()
        );
        let encoded = serde_json::to_string(&form).unwrap();
        assert_eq!(Form::parse(&encoded).unwrap(), form);
    }

    #[test]
    fn form_rejects_ambiguous_or_unbounded_input() {
        assert!(Form::parse(&FORM.replace("\"id\":\"q2\"", "\"id\":\"q1\"")).is_err());
        assert!(Form::parse(&FORM.replace("\"id\":\"b\"", "\"id\":\"a\"")).is_err());
        assert!(Form::parse(&FORM.replace("\"version\":1", "\"version\":2")).is_err());
        assert!(Form::parse(&"x".repeat(MAX_WIRE_BYTES + 1)).is_err());
        assert!(
            Form::parse(&FORM.replace(
                "\"question\":\"Which?\"",
                "\"question\":\"Which?\",\"extra\":true"
            ))
            .is_err()
        );
        assert!(
            Form::parse(&FORM.replace(
                "\"question\":\"Which?\"",
                "\"question\":\"Which?\",\"header\":\"你好你好你好你好你好你好\""
            ))
            .is_ok()
        );
        assert!(
            Form::parse(&FORM.replace(
                "\"question\":\"Which?\"",
                "\"question\":\"Which?\",\"header\":\"你好你好你好你好你好你好你\""
            ))
            .is_err()
        );
    }
}
