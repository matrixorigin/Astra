use std::collections::HashMap;

use astra_services::session_journal::ToolCallRecord;
use serde_json::Value;
use thiserror::Error;

pub(crate) const STOP_AFTER_SUCCESS_METADATA_KEY: &str = "stop_after_success";
pub(crate) const SUCCESS_FINAL_TEMPLATE_METADATA_KEY: &str = "success_final_template";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeStopAfterSuccessToolDescriptor {
    name: String,
    success_final_template: Option<String>,
}

impl RuntimeStopAfterSuccessToolDescriptor {
    pub(crate) fn from_metadata(
        public_name: &str,
        metadata: Option<&Value>,
    ) -> Result<Option<Self>, StopAfterSuccessMetadataError> {
        let Some(metadata) = metadata else {
            return Ok(None);
        };
        let metadata =
            metadata
                .as_object()
                .ok_or_else(|| StopAfterSuccessMetadataError::InvalidMetadata {
                    tool: public_name.to_string(),
                    detail: "metadata must be a JSON object".to_string(),
                })?;
        let Some(stop_after_success) = metadata.get(STOP_AFTER_SUCCESS_METADATA_KEY) else {
            return Ok(None);
        };
        let stop_after_success = stop_after_success.as_bool().ok_or_else(|| {
            StopAfterSuccessMetadataError::InvalidMetadata {
                tool: public_name.to_string(),
                detail: format!("metadata.{STOP_AFTER_SUCCESS_METADATA_KEY} must be a boolean"),
            }
        })?;
        if !stop_after_success {
            return Ok(None);
        }

        let success_final_template = match metadata.get(SUCCESS_FINAL_TEMPLATE_METADATA_KEY) {
            Some(value) => {
                let template = value.as_str().ok_or_else(|| {
                    StopAfterSuccessMetadataError::InvalidMetadata {
                        tool: public_name.to_string(),
                        detail: format!(
                            "metadata.{SUCCESS_FINAL_TEMPLATE_METADATA_KEY} must be a string"
                        ),
                    }
                })?;
                if template.trim().is_empty() {
                    return Err(StopAfterSuccessMetadataError::InvalidMetadata {
                        tool: public_name.to_string(),
                        detail: format!(
                            "metadata.{SUCCESS_FINAL_TEMPLATE_METADATA_KEY} must not be empty"
                        ),
                    });
                }
                Some(template.to_string())
            }
            None => None,
        };

        Ok(Some(Self {
            name: public_name.to_string(),
            success_final_template,
        }))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeStopAfterSuccessToolSnapshot {
    descriptors: HashMap<String, RuntimeStopAfterSuccessToolDescriptor>,
}

impl RuntimeStopAfterSuccessToolSnapshot {
    pub(crate) fn new(
        descriptors: impl IntoIterator<Item = RuntimeStopAfterSuccessToolDescriptor>,
    ) -> Self {
        Self {
            descriptors: descriptors
                .into_iter()
                .map(|descriptor| (descriptor.name.clone(), descriptor))
                .collect(),
        }
    }

    pub(crate) fn successful_tool_completion(
        &self,
        records: &[ToolCallRecord],
        results: &[Value],
    ) -> Option<crate::turn::agentic_loop::host::RuntimeSuccessfulToolCompletion> {
        records.iter().find_map(|record| {
            if !record.ok {
                return None;
            }
            let descriptor = self.descriptors.get(&record.name)?;
            let call_id = record.tool_call_id.as_deref()?;
            let result = results.iter().find(|result| {
                result.get("tool_call_id").and_then(Value::as_str) == Some(call_id)
            })?;
            let output = result
                .get("structuredContent")
                .and_then(Value::as_object)
                .and_then(|content| content.get("output"))
                .and_then(Value::as_object)?;
            if output.get("ok").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            Some(
                crate::turn::agentic_loop::host::RuntimeSuccessfulToolCompletion {
                    tool_name: record.name.clone(),
                    final_text: descriptor
                        .success_final_template
                        .as_deref()
                        .and_then(|template| render_success_final_template(template, output)),
                },
            )
        })
    }
}

fn render_success_final_template(
    template: &str,
    output: &serde_json::Map<String, Value>,
) -> Option<String> {
    let mut text = template.to_string();
    for key in ["summary", "message", "answer"] {
        let value = output.get(key).and_then(Value::as_str).unwrap_or_default();
        text = text.replace(&format!("{{{{{key}}}}}"), value.trim());
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
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
        let descriptor = RuntimeStopAfterSuccessToolDescriptor::from_metadata(
            "tool",
            Some(&json!({
                "stop_after_success": true,
                "success_final_template": "{{message}}"
            })),
        )
        .unwrap()
        .expect("descriptor");
        assert_eq!(descriptor.name, "tool");
        assert_eq!(
            descriptor.success_final_template.as_deref(),
            Some("{{message}}")
        );
        let descriptor_without_template = RuntimeStopAfterSuccessToolDescriptor::from_metadata(
            "tool",
            Some(&json!({"stop_after_success": true})),
        )
        .unwrap()
        .expect("descriptor");
        assert!(descriptor_without_template.success_final_template.is_none());
        assert!(
            RuntimeStopAfterSuccessToolDescriptor::from_metadata(
                "tool",
                Some(&json!({
                    "stop_after_success": false
                }))
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn rejects_non_boolean_stop_after_success_metadata() {
        let error = RuntimeStopAfterSuccessToolDescriptor::from_metadata(
            "tool",
            Some(&json!({
                "stop_after_success": "true"
            })),
        )
        .unwrap_err();
        assert_eq!(error.error_code(), "stop_after_success_contract_violation");
    }

    #[test]
    fn rejects_non_string_success_final_template_metadata() {
        let error = RuntimeStopAfterSuccessToolDescriptor::from_metadata(
            "tool",
            Some(&json!({
                "stop_after_success": true,
                "success_final_template": true
            })),
        )
        .unwrap_err();
        assert_eq!(error.error_code(), "stop_after_success_contract_violation");
    }

    #[test]
    fn rejects_empty_success_final_template_metadata() {
        let error = RuntimeStopAfterSuccessToolDescriptor::from_metadata(
            "tool",
            Some(&json!({
                "stop_after_success": true,
                "success_final_template": "  "
            })),
        )
        .unwrap_err();
        assert_eq!(error.error_code(), "stop_after_success_contract_violation");
    }

    #[test]
    fn stops_only_for_successful_structured_tool_result() {
        let snapshot = RuntimeStopAfterSuccessToolSnapshot::new([
            RuntimeStopAfterSuccessToolDescriptor::from_metadata(
                "mcp__moi__agent_builder",
                Some(&json!({
                    "stop_after_success": true,
                    "success_final_template": "Parsing started: {{message}}"
                })),
            )
            .unwrap()
            .unwrap(),
        ]);
        let records = vec![successful_record("mcp__moi__agent_builder", "call-1")];
        let success = vec![json!({
            "tool_call_id": "call-1",
            "structuredContent": {"output": {"ok": true, "message": "workflow-1"}}
        })];
        let completion = snapshot
            .successful_tool_completion(&records, &success)
            .expect("successful completion");
        assert_eq!(completion.tool_name, "mcp__moi__agent_builder");
        assert_eq!(
            completion.final_text.as_deref(),
            Some("Parsing started: workflow-1")
        );

        let invalid = vec![json!({
            "tool_call_id": "call-1",
            "structuredContent": {"output": {"ok": false}}
        })];
        assert!(
            snapshot
                .successful_tool_completion(&records, &invalid)
                .is_none()
        );
    }
}
