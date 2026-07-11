use std::collections::HashSet;

use astra_services::session_journal::ToolCallRecord;
use serde_json::Value;
use thiserror::Error;

pub(crate) const STOP_AFTER_SUCCESS_METADATA_KEY: &str = "stop_after_success";

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeStopAfterSuccessToolSnapshot {
    names: HashSet<String>,
}

impl RuntimeStopAfterSuccessToolSnapshot {
    pub(crate) fn new(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }

    pub(crate) fn successful_tool_name(
        &self,
        records: &[ToolCallRecord],
        results: &[Value],
    ) -> Option<String> {
        records.iter().find_map(|record| {
            if !record.ok || !self.names.contains(&record.name) {
                return None;
            }
            let call_id = record.tool_call_id.as_deref()?;
            let result = results.iter().find(|result| {
                result.get("tool_call_id").and_then(Value::as_str) == Some(call_id)
            })?;
            let succeeded = result
                .get("structuredContent")
                .and_then(Value::as_object)
                .and_then(|content| content.get("output"))
                .and_then(Value::as_object)
                .and_then(|output| output.get("ok"))
                .and_then(Value::as_bool)
                == Some(true);
            succeeded.then(|| record.name.clone())
        })
    }
}

pub(crate) fn stop_after_success_from_metadata(
    public_name: &str,
    metadata: Option<&Value>,
) -> Result<bool, StopAfterSuccessMetadataError> {
    let Some(metadata) = metadata else {
        return Ok(false);
    };
    let metadata =
        metadata
            .as_object()
            .ok_or_else(|| StopAfterSuccessMetadataError::InvalidMetadata {
                tool: public_name.to_string(),
                detail: "metadata must be a JSON object".to_string(),
            })?;
    let Some(value) = metadata.get(STOP_AFTER_SUCCESS_METADATA_KEY) else {
        return Ok(false);
    };
    value
        .as_bool()
        .ok_or_else(|| StopAfterSuccessMetadataError::InvalidMetadata {
            tool: public_name.to_string(),
            detail: format!("metadata.{STOP_AFTER_SUCCESS_METADATA_KEY} must be a boolean"),
        })
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum StopAfterSuccessMetadataError {
    #[error("stop-after-success descriptor for tool '{tool}' is invalid: {detail}")]
    InvalidMetadata { tool: String, detail: String },
}

impl StopAfterSuccessMetadataError {
    pub(crate) fn error_code(&self) -> &'static str {
        "stop_after_success_contract_violation"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn successful_record(name: &str, call_id: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            tool_call_id: Some(call_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn parses_stop_after_success_metadata() {
        assert!(
            stop_after_success_from_metadata(
                "tool",
                Some(&json!({
                    "stop_after_success": true
                }))
            )
            .unwrap()
        );
        assert!(
            !stop_after_success_from_metadata(
                "tool",
                Some(&json!({
                    "stop_after_success": false
                }))
            )
            .unwrap()
        );
    }

    #[test]
    fn rejects_non_boolean_stop_after_success_metadata() {
        let error = stop_after_success_from_metadata(
            "tool",
            Some(&json!({
                "stop_after_success": "true"
            })),
        )
        .unwrap_err();
        assert_eq!(error.error_code(), "stop_after_success_contract_violation");
    }

    #[test]
    fn stops_only_for_successful_structured_tool_result() {
        let snapshot =
            RuntimeStopAfterSuccessToolSnapshot::new(["mcp__moi__agent_builder".to_string()]);
        let records = vec![successful_record("mcp__moi__agent_builder", "call-1")];
        let success = vec![json!({
            "tool_call_id": "call-1",
            "structuredContent": {"output": {"ok": true}}
        })];
        assert_eq!(
            snapshot.successful_tool_name(&records, &success).as_deref(),
            Some("mcp__moi__agent_builder")
        );

        let invalid = vec![json!({
            "tool_call_id": "call-1",
            "structuredContent": {"output": {"ok": false}}
        })];
        assert!(snapshot.successful_tool_name(&records, &invalid).is_none());
    }
}
