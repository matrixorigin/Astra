//! Pure, versioned task criteria. Execution status is not a verifier verdict.

use astra_core::canonical_json_string;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::execution::content_fingerprint;

pub const JSON_VALUE_EQUALS_ID: &str = "json_value_equals";
pub const JSON_VALUE_EQUALS_VERSION: &str = "1";
const IMPLEMENTATION: &str = "json_value_equals.v1: complete JSON document; root Value equality; object order irrelevant; array order significant; integer and float representations distinct; no Markdown or prose; max expected canonical UTF-8 bytes=65536; max output UTF-8 bytes=1048576";
const RUBRIC: &str = "The complete assistant output must parse as one JSON value and equal the frozen expected value. This verifies only that structured-output criterion.";
const MAX_EXPECTED_BYTES: usize = 65_536;
pub(super) const MAX_OUTPUT_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonValueEqualsConfig {
    pub expected: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskVerifierSpec {
    pub implementation_id: String,
    pub implementation_version: String,
    pub implementation_hash: String,
    pub rubric_hash: String,
    pub config: JsonValueEqualsConfig,
    pub config_hash: String,
}

impl TaskVerifierSpec {
    pub fn freeze(config: JsonValueEqualsConfig) -> Result<Self, String> {
        let canonical =
            canonical_json_string(&serde_json::to_value(&config).map_err(|e| e.to_string())?);
        if canonical.len() > MAX_EXPECTED_BYTES {
            return Err("task verifier expected configuration exceeds 65536 bytes".into());
        }
        Ok(Self {
            implementation_id: JSON_VALUE_EQUALS_ID.into(),
            implementation_version: JSON_VALUE_EQUALS_VERSION.into(),
            implementation_hash: content_fingerprint(IMPLEMENTATION),
            rubric_hash: content_fingerprint(RUBRIC),
            config_hash: content_fingerprint(&canonical),
            config,
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if *self != Self::freeze(self.config.clone())? {
            return Err(
                "task verifier implementation, rubric, or configuration identity mismatch".into(),
            );
        }
        Ok(())
    }
}

/// Shared with the harness; parsing consumes the entire output, including
/// trailing-input validation. No Markdown extraction or prose heuristics.
pub fn parse_complete_json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text.trim())
        .map_err(|error| format!("assistant text is not exactly one JSON value: {error}"))
}

pub fn verify_json_pointer(document: &Value, path: &str, expected: &Value) -> Result<(), String> {
    let actual = document
        .pointer(path)
        .ok_or_else(|| format!("JSON pointer {path:?} is absent"))?;
    if actual != expected {
        return Err(format!(
            "JSON pointer {path:?} did not equal its expected value"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskVerifierVerdict {
    Pass,
    Fail,
    Unavailable,
}

/// Call only with complete output resolved through a trusted persisted output
/// receipt. This pure function itself makes no provenance or ownership claim.
pub fn verify_complete_output(
    spec: &TaskVerifierSpec,
    output: Option<&str>,
) -> Result<TaskVerifierVerdict, String> {
    spec.validate()?;
    let Some(output) = output.filter(|text| text.len() <= MAX_OUTPUT_BYTES) else {
        return Ok(TaskVerifierVerdict::Unavailable);
    };
    Ok(
        if parse_complete_json(output)
            .and_then(|value| verify_json_pointer(&value, "", &spec.config.expected))
            .is_ok()
        {
            TaskVerifierVerdict::Pass
        } else {
            TaskVerifierVerdict::Fail
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn complete_json_is_required_and_missing_output_is_not_failure() {
        let spec = TaskVerifierSpec::freeze(JsonValueEqualsConfig {
            expected: json!({"ok": true}),
        })
        .unwrap();
        for output in ["{\"ok\":true}", " \n{\"ok\":true}\n"] {
            assert_eq!(
                verify_complete_output(&spec, Some(output)).unwrap(),
                TaskVerifierVerdict::Pass
            );
        }
        for output in [
            "",
            "{}",
            "```json\n{\"ok\":true}\n```",
            "{\"ok\":true} {}",
            "done",
        ] {
            assert_eq!(
                verify_complete_output(&spec, Some(output)).unwrap(),
                TaskVerifierVerdict::Fail
            );
        }
        assert_eq!(
            verify_complete_output(&spec, None).unwrap(),
            TaskVerifierVerdict::Unavailable
        );
        assert_eq!(
            verify_complete_output(&spec, Some(&" ".repeat(MAX_OUTPUT_BYTES + 1))).unwrap(),
            TaskVerifierVerdict::Unavailable
        );
    }

    #[test]
    fn frozen_configuration_and_semantics_are_validated() {
        let mut spec = TaskVerifierSpec::freeze(JsonValueEqualsConfig {
            expected: json!([1, 2]),
        })
        .unwrap();
        assert_eq!(
            verify_complete_output(&spec, Some("[2,1]")).unwrap(),
            TaskVerifierVerdict::Fail
        );
        assert_eq!(
            verify_complete_output(&spec, Some("[1.0,2]")).unwrap(),
            TaskVerifierVerdict::Fail
        );
        spec.config.expected = json!([2, 1]);
        assert!(spec.validate().is_err());
        assert!(
            TaskVerifierSpec::freeze(JsonValueEqualsConfig {
                expected: json!("x".repeat(MAX_EXPECTED_BYTES))
            })
            .is_err()
        );
    }
}
