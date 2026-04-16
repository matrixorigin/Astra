use std::collections::BTreeSet;

use serde_json::{Map, Value};

pub fn latest_assistant_tool_call_ids(history: &[Map<String, Value>]) -> BTreeSet<String> {
    history
        .iter()
        .rev()
        .find_map(|message| {
            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|tool_calls| {
                        tool_calls
                            .iter()
                            .filter_map(|tool_call| {
                                tool_call
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                            })
                            .collect::<BTreeSet<_>>()
                    })
            } else {
                None
            }
        })
        .unwrap_or_default()
}

pub fn build_unconsumed_tool_messages(
    tool_results: &[Value],
    consumed: &BTreeSet<String>,
    allowed_tool_call_ids: &BTreeSet<String>,
) -> Vec<Map<String, Value>> {
    tool_results
        .iter()
        .filter_map(|tool_result| {
            let object = tool_result.as_object()?;
            let tool_call_id = object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if tool_call_id.is_empty()
                || consumed.contains(tool_call_id)
                || !allowed_tool_call_ids.contains(tool_call_id)
            {
                return None;
            }
            let raw_content = object.get("result").map(|value| match value {
                Value::String(text) => text.clone(),
                other => json_stringify(other),
            })?;
            // Prompt injection guard: delimit tool output so the LLM treats
            // it as data, not instructions.  Any injected "system" directives
            // inside tool output are contained within these markers.
            let content = format!("[TOOL OUTPUT]\n{}\n[/TOOL OUTPUT]", raw_content);
            Some(Map::from_iter([
                ("role".to_string(), Value::from("tool")),
                ("tool_call_id".to_string(), Value::from(tool_call_id)),
                ("content".to_string(), Value::from(content)),
            ]))
        })
        .collect()
}

fn json_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- latest_assistant_tool_call_ids ---

    #[test]
    fn latest_ids_empty_history() {
        assert!(latest_assistant_tool_call_ids(&[]).is_empty());
    }

    #[test]
    fn latest_ids_no_assistant() {
        let history = vec![Map::from_iter([("role".to_string(), json!("user"))])];
        assert!(latest_assistant_tool_call_ids(&history).is_empty());
    }

    #[test]
    fn latest_ids_assistant_no_tool_calls() {
        let history = vec![Map::from_iter([
            ("role".to_string(), json!("assistant")),
            ("content".to_string(), json!("hello")),
        ])];
        assert!(latest_assistant_tool_call_ids(&history).is_empty());
    }

    #[test]
    fn latest_ids_extracts_from_last_assistant() {
        let history = vec![
            Map::from_iter([
                ("role".to_string(), json!("assistant")),
                ("tool_calls".to_string(), json!([{"id": "old"}])),
            ]),
            Map::from_iter([("role".to_string(), json!("user"))]),
            Map::from_iter([
                ("role".to_string(), json!("assistant")),
                (
                    "tool_calls".to_string(),
                    json!([{"id": "tc1"}, {"id": "tc2"}]),
                ),
            ]),
        ];
        let ids = latest_assistant_tool_call_ids(&history);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("tc1"));
        assert!(ids.contains("tc2"));
    }

    #[test]
    fn latest_ids_skips_non_string_ids() {
        let history = vec![Map::from_iter([
            ("role".to_string(), json!("assistant")),
            (
                "tool_calls".to_string(),
                json!([{"id": 42}, {"id": "valid"}]),
            ),
        ])];
        let ids = latest_assistant_tool_call_ids(&history);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("valid"));
    }

    // --- build_unconsumed_tool_messages ---

    #[test]
    fn unconsumed_empty_results() {
        let consumed = BTreeSet::new();
        let allowed = BTreeSet::new();
        assert!(build_unconsumed_tool_messages(&[], &consumed, &allowed).is_empty());
    }

    #[test]
    fn unconsumed_filters_consumed() {
        let consumed = BTreeSet::from(["tc1".to_string()]);
        let allowed = BTreeSet::from(["tc1".to_string()]);
        let results = vec![json!({"tool_call_id": "tc1", "result": "ok"})];
        assert!(build_unconsumed_tool_messages(&results, &consumed, &allowed).is_empty());
    }

    #[test]
    fn unconsumed_filters_not_allowed() {
        let consumed = BTreeSet::new();
        let allowed = BTreeSet::from(["tc2".to_string()]);
        let results = vec![json!({"tool_call_id": "tc1", "result": "ok"})];
        assert!(build_unconsumed_tool_messages(&results, &consumed, &allowed).is_empty());
    }

    #[test]
    fn unconsumed_filters_empty_tool_call_id() {
        let consumed = BTreeSet::new();
        let allowed = BTreeSet::from(["".to_string()]);
        let results = vec![json!({"tool_call_id": "", "result": "ok"})];
        assert!(build_unconsumed_tool_messages(&results, &consumed, &allowed).is_empty());
    }

    #[test]
    fn unconsumed_builds_tool_message() {
        let consumed = BTreeSet::new();
        let allowed = BTreeSet::from(["tc1".to_string()]);
        let results = vec![json!({"tool_call_id": "tc1", "result": "data"})];
        let msgs = build_unconsumed_tool_messages(&results, &consumed, &allowed);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "tool");
        assert_eq!(msgs[0]["tool_call_id"].as_str().unwrap(), "tc1");
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(content.starts_with("[TOOL OUTPUT]"));
        assert!(content.contains("data"));
        assert!(content.ends_with("[/TOOL OUTPUT]"));
    }

    #[test]
    fn unconsumed_non_string_result_serialized() {
        let consumed = BTreeSet::new();
        let allowed = BTreeSet::from(["tc1".to_string()]);
        let results = vec![json!({"tool_call_id": "tc1", "result": {"key": "val"}})];
        let msgs = build_unconsumed_tool_messages(&results, &consumed, &allowed);
        assert_eq!(msgs.len(), 1);
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(content.contains(r#""key":"val""#));
    }

    #[test]
    fn unconsumed_skips_missing_result() {
        let consumed = BTreeSet::new();
        let allowed = BTreeSet::from(["tc1".to_string()]);
        let results = vec![json!({"tool_call_id": "tc1"})];
        assert!(build_unconsumed_tool_messages(&results, &consumed, &allowed).is_empty());
    }

    #[test]
    fn unconsumed_skips_non_object() {
        let consumed = BTreeSet::new();
        let allowed = BTreeSet::new();
        let results = vec![json!("not an object")];
        assert!(build_unconsumed_tool_messages(&results, &consumed, &allowed).is_empty());
    }

    // --- json_stringify ---

    #[test]
    fn stringify_null() {
        assert_eq!(json_stringify(&json!(null)), "null");
    }

    #[test]
    fn stringify_object() {
        let result = json_stringify(&json!({"a": 1}));
        assert!(result.contains("\"a\""));
    }
}
