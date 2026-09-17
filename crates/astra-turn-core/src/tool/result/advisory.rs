//! Runtime guidance is presentation, not part of the executor's result document.

use serde_json::{Map, Value};

/// Internal, per-call metadata retained in canonical history but not sent as a
/// provider message field. Provider projection consumes it exactly once.
pub const TOOL_RESULT_ADVISORIES_FIELD: &str = "_astra_tool_result_advisories";

pub fn advisories(fields: Option<&Map<String, Value>>) -> Vec<String> {
    fields
        .and_then(|fields| fields.get(TOOL_RESULT_ADVISORIES_FIELD))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

/// Detach host guidance before handing executor metadata to durable completion.
pub fn take_advisories(fields: Option<&mut Map<String, Value>>) -> Vec<String> {
    fields
        .and_then(|fields| fields.remove(TOOL_RESULT_ADVISORIES_FIELD))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

pub fn set_advisories(fields: &mut Map<String, Value>, guidance: &[String]) {
    if guidance.is_empty() {
        fields.remove(TOOL_RESULT_ADVISORIES_FIELD);
    } else {
        fields.insert(
            TOOL_RESULT_ADVISORIES_FIELD.into(),
            Value::from(guidance.to_vec()),
        );
    }
}

/// Render a display-only copy. Canonical tool documents must never use this path.
pub fn append_display_guidance(display: &mut String, guidance: &[String]) {
    if !guidance.is_empty() {
        display.push_str(&format!(
            "\n\n[Runtime tool guidance]\n{}",
            guidance.join("\n")
        ));
    }
}

/// Apply only to a disposable provider projection, never canonical history.
/// Removing the metadata makes repeated projection idempotent without parsing
/// or matching any provider-authored text.
pub fn project_advisories(message: &mut Value) {
    let Some(object) = message.as_object_mut() else {
        return;
    };
    let guidance = advisories(Some(object));
    object.remove(TOOL_RESULT_ADVISORIES_FIELD);
    if object.get("role").and_then(Value::as_str) != Some("tool") || guidance.is_empty() {
        return;
    }
    let mut display = String::new();
    append_display_guidance(&mut display, &guidance);
    match object.get_mut("content") {
        Some(Value::String(content)) => content.push_str(&display),
        Some(Value::Array(blocks)) => {
            blocks.push(serde_json::json!({"type":"text", "text":display}))
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_projection_preserves_canonical_document_and_is_idempotent() {
        for body in [
            r#"{ "executed": false }"#,
            "[1,2]",
            "null",
            r#""error""#,
            "plain text",
        ] {
            let canonical = json!({"role":"tool", "tool_call_id":"call-1", "content":body,
                TOOL_RESULT_ADVISORIES_FIELD: ["correct the arguments"]});
            let mut projected = canonical.clone();
            project_advisories(&mut projected);
            assert_eq!(canonical["content"], body);
            assert!(
                projected["content"]
                    .as_str()
                    .unwrap()
                    .contains("correct the arguments")
            );
            assert!(projected.get(TOOL_RESULT_ADVISORIES_FIELD).is_none());
            let once = projected.clone();
            project_advisories(&mut projected);
            assert_eq!(projected, once);
        }
    }
}
