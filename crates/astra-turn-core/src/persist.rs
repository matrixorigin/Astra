use serde_json::{Map, Value, json};

use super::action_compensation::tool_action_profile_value;

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
        ("tool_name".to_string(), Value::from(skill_name.clone())),
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
    let raw_arguments = function
        .get("arguments")
        .cloned()
        .unwrap_or(Value::from("{}"));
    let mut content = Map::from_iter([
        (
            "tool_call_id".to_string(),
            tool_call.get("id").cloned().unwrap_or(Value::from("")),
        ),
        ("name".to_string(), Value::from(skill_name.clone())),
        ("arguments".to_string(), raw_arguments.clone()),
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
            ("tool_name".to_string(), Value::from(skill_name.clone())),
            ("source".to_string(), Value::from(source)),
            (
                "action_profile".to_string(),
                tool_action_profile_value(&skill_name, &raw_arguments),
            ),
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- truncate_chars ---

    #[test]
    fn truncate_zero_limit() {
        assert_eq!(truncate_chars("hello", 0), "");
    }

    #[test]
    fn truncate_within_limit() {
        assert_eq!(truncate_chars("hi", 10), "hi");
    }

    #[test]
    fn truncate_exact_limit() {
        assert_eq!(truncate_chars("abc", 3), "abc");
    }

    #[test]
    fn truncate_over_limit() {
        assert_eq!(truncate_chars("abcdef", 3), "abc");
    }

    #[test]
    fn truncate_multibyte() {
        assert_eq!(truncate_chars("你好世界", 2), "你好");
    }

    #[test]
    fn truncate_empty() {
        assert_eq!(truncate_chars("", 5), "");
    }

    // --- build_llm_response_persist_plan ---

    #[test]
    fn persist_plan_empty_no_calls() {
        let plan = build_llm_response_persist_plan("", false, "");
        assert!(!plan.should_persist);
        assert!(plan.content.is_empty());
        assert!(plan.reasoning_content.is_none());
    }

    #[test]
    fn persist_plan_empty_with_calls() {
        let plan = build_llm_response_persist_plan("", true, "");
        assert!(plan.should_persist);
    }

    #[test]
    fn persist_plan_whitespace_only() {
        let plan = build_llm_response_persist_plan("  \n  ", false, "");
        assert!(!plan.should_persist);
        assert!(plan.content.is_empty());
    }

    #[test]
    fn persist_plan_text_no_calls_with_reasoning() {
        let plan = build_llm_response_persist_plan("hello", false, "think");
        assert!(plan.should_persist);
        assert_eq!(plan.content, "hello");
        assert_eq!(plan.reasoning_content.as_deref(), Some("think"));
    }

    #[test]
    fn persist_plan_text_with_calls_no_reasoning() {
        let plan = build_llm_response_persist_plan("hello", true, "think");
        assert!(plan.should_persist);
        // reasoning suppressed when has_tool_calls
        assert!(plan.reasoning_content.is_none());
    }

    // --- build_tool_result_event_payload ---

    #[test]
    fn tool_result_missing_name() {
        let tr = Map::new();
        let p = build_tool_result_event_payload(&tr, "edge", 100);
        assert!(p.skill_name.is_empty());
        assert_eq!(p.metadata["source"].as_str().unwrap(), "edge");
    }

    #[test]
    fn tool_result_edge_introspection() {
        let tr = Map::from_iter([("name".to_string(), json!("get_agent_info"))]);
        let p = build_tool_result_event_payload(&tr, "edge", 100);
        assert_eq!(p.metadata["introspection"].as_bool(), Some(true));
    }

    #[test]
    fn tool_result_cloud_no_introspection() {
        let tr = Map::from_iter([("name".to_string(), json!("get_agent_info"))]);
        let p = build_tool_result_event_payload(&tr, "cloud", 100);
        assert!(p.metadata.get("introspection").is_none());
    }

    #[test]
    fn tool_result_truncates_result() {
        let tr = Map::from_iter([("result".to_string(), json!("abcdefghij"))]);
        let p = build_tool_result_event_payload(&tr, "edge", 5);
        assert_eq!(p.content["result"].as_str().unwrap(), "abcde");
    }

    #[test]
    fn tool_result_missing_result() {
        let tr = Map::new();
        let p = build_tool_result_event_payload(&tr, "edge", 100);
        assert_eq!(p.content["result"].as_str().unwrap(), "");
    }

    #[test]
    fn tool_result_tool_call_id_preserved() {
        let tr = Map::from_iter([("tool_call_id".to_string(), json!("tc_123"))]);
        let p = build_tool_result_event_payload(&tr, "edge", 100);
        assert_eq!(p.metadata["tool_call_id"].as_str().unwrap(), "tc_123");
    }

    // --- build_tool_call_event_payload ---

    #[test]
    fn tool_call_missing_function() {
        let tc = Map::new();
        let p = build_tool_call_event_payload(&tc, 0, "");
        assert!(p.skill_name.is_empty());
        assert_eq!(p.content["arguments"].as_str().unwrap(), "{}");
    }

    #[test]
    fn tool_call_cloud_source() {
        let tc = Map::from_iter([
            ("_source".to_string(), json!("cloud")),
            ("function".to_string(), json!({"name": "bash"})),
        ]);
        let p = build_tool_call_event_payload(&tc, 0, "");
        assert_eq!(p.content["source"].as_str().unwrap(), "cloud");
        assert_eq!(p.metadata["source"].as_str().unwrap(), "cloud");
    }

    #[test]
    fn tool_call_edge_source_no_source_field() {
        let tc = Map::from_iter([("function".to_string(), json!({"name": "bash"}))]);
        let p = build_tool_call_event_payload(&tc, 0, "");
        assert!(p.content.get("source").is_none());
        assert_eq!(p.metadata["source"].as_str().unwrap(), "edge");
    }

    #[test]
    fn tool_call_reasoning_only_at_index_0() {
        let tc = Map::from_iter([("function".to_string(), json!({"name": "x"}))]);
        let p0 = build_tool_call_event_payload(&tc, 0, "think");
        assert_eq!(p0.reasoning_content.as_deref(), Some("think"));

        let p1 = build_tool_call_event_payload(&tc, 1, "think");
        assert!(p1.reasoning_content.is_none());
    }

    #[test]
    fn tool_call_empty_reasoning_at_index_0() {
        let tc = Map::from_iter([("function".to_string(), json!({"name": "x"}))]);
        let p = build_tool_call_event_payload(&tc, 0, "");
        assert!(p.reasoning_content.is_none());
    }

    #[test]
    fn tool_call_id_defaults_empty() {
        let tc = Map::new();
        let p = build_tool_call_event_payload(&tc, 0, "");
        assert_eq!(p.content["tool_call_id"].as_str().unwrap(), "");
        assert_eq!(p.metadata["tool_call_id"].as_str().unwrap(), "");
    }

    #[test]
    fn tool_call_metadata_carries_action_profile() {
        let tc = Map::from_iter([(
            "function".to_string(),
            json!({"name": "write_file", "arguments": {"path": "src/main.rs"}}),
        )]);
        let p = build_tool_call_event_payload(&tc, 0, "");
        assert_eq!(
            p.metadata["action_profile"]["category"].as_str(),
            Some("write")
        );
        assert_eq!(
            p.metadata["action_profile"]["compensation_kind"].as_str(),
            Some("restore_or_delete_file")
        );
        assert_eq!(
            p.metadata["action_profile"]["requires_pre_state"].as_bool(),
            Some(true)
        );
    }
}
