use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct PersistEventPayload {
    pub content: Value,
    pub metadata: Map<String, Value>,
    pub skill_name: String,
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmResponsePersistPlan {
    pub should_persist: bool,
    pub content: String,
    pub reasoning_content: Option<String>,
}

pub fn build_tool_result_event_payload(
    tool_result: &Map<String, Value>,
    source: &str,
    audit_chars: usize,
) -> PersistEventPayload {
    let skill_name = tool_result
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut metadata = Map::from_iter([
        ("source".to_string(), Value::from(source)),
        (
            "tool_call_id".to_string(),
            tool_result
                .get("tool_call_id")
                .cloned()
                .unwrap_or(Value::Null),
        ),
        ("name".to_string(), Value::from(skill_name.clone())),
    ]);
    if source == "edge" && skill_name == "get_agent_info" {
        metadata.insert("introspection".to_string(), Value::Bool(true));
    }
    let result = tool_result
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("");
    PersistEventPayload {
        content: json!({
            "name": skill_name,
            "result": truncate_chars(result, audit_chars),
        }),
        metadata,
        skill_name,
        reasoning_content: None,
    }
}

pub fn build_tool_call_event_payload(
    tool_call: &Map<String, Value>,
    index: usize,
    reasoning_content: &str,
) -> PersistEventPayload {
    let function = tool_call
        .get("function")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let skill_name = function
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let source = tool_call
        .get("_source")
        .and_then(Value::as_str)
        .unwrap_or("edge");
    let mut content = Map::from_iter([
        (
            "tool_call_id".to_string(),
            tool_call.get("id").cloned().unwrap_or(Value::from("")),
        ),
        ("name".to_string(), Value::from(skill_name.clone())),
        (
            "arguments".to_string(),
            function
                .get("arguments")
                .cloned()
                .unwrap_or(Value::from("{}")),
        ),
    ]);
    if source == "cloud" {
        content.insert("source".to_string(), Value::from("cloud"));
    }
    PersistEventPayload {
        content: Value::Object(content),
        metadata: Map::from_iter([
            (
                "tool_call_id".to_string(),
                tool_call.get("id").cloned().unwrap_or(Value::from("")),
            ),
            ("name".to_string(), Value::from(skill_name.clone())),
            ("source".to_string(), Value::from(source)),
        ]),
        skill_name,
        reasoning_content: if !reasoning_content.is_empty() && index == 0 {
            Some(reasoning_content.to_string())
        } else {
            None
        },
    }
}

pub fn build_llm_response_persist_plan(
    full_text: &str,
    has_tool_calls: bool,
    reasoning_content: &str,
) -> LlmResponsePersistPlan {
    let content = full_text.trim().to_string();
    LlmResponsePersistPlan {
        should_persist: !content.is_empty() || has_tool_calls,
        content,
        reasoning_content: if !reasoning_content.is_empty() && !has_tool_calls {
            Some(reasoning_content.to_string())
        } else {
            None
        },
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
