//! Match cloud assistant `tool_calls` to edge-executed `tool_request` rows (§5.5 headless path).
//!
//! Shared between the CLI SSE loop and any future server-side handler that consumes the same shape.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::tool_result_semantics::tool_dedup_signature;

/// Tools that are idempotent reads — safe to cache across turns.
/// Side-effectful tools must not appear here.
pub const CACHEABLE_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "grep",
    "glob",
    "symbols",
    "find_definition",
    "find_references",
    "git_status",
    "git_diff",
    "git_log",
    "git_blame",
    "git_file_history",
    "git_contributors",
    "git_log_search",
    "github_list_prs",
    "github_get_pr",
    "github_ci_status",
    "github_list_issues",
    "github_get_issue",
    "get_agent_info",
];

/// One edge-executed tool row in the current LLM round (ordering preserved vs `tool_calls`).
pub trait EdgeToolRoundRow {
    fn tool_name(&self) -> &str;
    fn tool_args(&self) -> &Value;
    fn tool_output(&self) -> &str;

    /// OpenAI `tool_calls[].id` when synthesizing from an edge-only round (§5.5).
    /// Default `edge-{index}`; rows with a server `request_id` should override.
    fn assistant_tool_call_id(&self, index: usize) -> String {
        format!("edge-{index}")
    }
}

/// Take output for a server-emitted `tool_call` by matching dedup signature against the edge round.
pub fn take_edge_output_for_tool_call<T: EdgeToolRoundRow>(
    name: &str,
    args: &Value,
    round: &[T],
    consumed: &mut [bool],
    by_sig: &HashMap<String, String>,
) -> String {
    let sig = tool_dedup_signature(name, args);
    for (i, e) in round.iter().enumerate() {
        if consumed.get(i).copied().unwrap_or(true) {
            continue;
        }
        if tool_dedup_signature(e.tool_name(), e.tool_args()) == sig {
            consumed[i] = true;
            return e.tool_output().to_string();
        }
    }
    by_sig.get(&sig).cloned().unwrap_or_else(|| {
        format!(
            "Error: headless edge protocol — expected SSE `tool_request` before assistant `tool_call` for `{name}` (no matching edge execution in this turn)."
        )
    })
}

/// Normalize server `tool_calls` or synthetic edge-round rows for stall / TurnGuard signature tracking.
pub fn tool_calls_for_stall_guard<T: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[T],
) -> Vec<Value> {
    if !server_tool_calls.is_empty() {
        server_tool_calls.to_vec()
    } else {
        // Match historical CLI behavior: stall/TurnGuard sees synthetic ids `edge-{i}` only
        // (OpenAI-shaped assistant `tool_calls` may still use `request_id` elsewhere).
        edge_round
            .iter()
            .enumerate()
            .map(|(i, e)| {
                json!({
                    "id": format!("edge-{i}"),
                    "name": e.tool_name(),
                    "arguments": e.tool_args().clone(),
                })
            })
            .collect()
    }
}

fn openai_tool_call_entries_from_server(tool_calls: &[Value]) -> Vec<Value> {
    tool_calls
        .iter()
        .map(|tc| {
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = tc.get("arguments").cloned().unwrap_or(json!({}));
            json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(&args)
                        .unwrap_or_else(|_| r#"{"error":"argument serialization failed"}"#.to_string()),
                }
            })
        })
        .collect()
}

/// Assistant message with `content: null` and OpenAI-shaped `tool_calls` (server list or edge round).
pub fn openai_assistant_with_tool_calls_message<T: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[T],
    reasoning_content: &str,
) -> Value {
    let mut msg = if !server_tool_calls.is_empty() {
        json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": openai_tool_call_entries_from_server(server_tool_calls),
        })
    } else {
        let items: Vec<Value> = edge_round
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let id = e.assistant_tool_call_id(i);
                json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": e.tool_name(),
                        "arguments": serde_json::to_string(e.tool_args())
                            .unwrap_or_else(|_| "{}".to_string()),
                    }
                })
            })
            .collect();
        json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": items,
        })
    };
    if !reasoning_content.is_empty() && let Some(obj) = msg.as_object_mut() {
        obj.insert(
            "reasoning_content".to_string(),
            Value::String(reasoning_content.to_string()),
        );
    }
    msg
}

/// OpenAI `role: "tool"` message plus matching `/chat` `tool_results` row (`content` / `result` identical).
#[must_use]
pub fn openai_tool_roundtrip_values(
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> (Value, Value) {
    let msg = json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": content,
    });
    let tr = json!({
        "tool_call_id": tool_call_id,
        "name": tool_name,
        "result": content,
    });
    (msg, tr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Row {
        tool: String,
        args: Value,
        output: String,
    }

    impl EdgeToolRoundRow for Row {
        fn tool_name(&self) -> &str {
            &self.tool
        }
        fn tool_args(&self) -> &Value {
            &self.args
        }
        fn tool_output(&self) -> &str {
            &self.output
        }
    }

    #[test]
    fn take_edge_output_matches_first_unconsumed_row() {
        let rows = vec![
            Row {
                tool: "read_file".into(),
                args: json!({"path": "x.rs"}),
                output: "one".into(),
            },
            Row {
                tool: "read_file".into(),
                args: json!({"path": "y.rs"}),
                output: "two".into(),
            },
        ];
        let mut consumed = vec![false; 2];
        let by_sig: HashMap<String, String> = HashMap::new();
        let out = take_edge_output_for_tool_call(
            "read_file",
            &json!({"path": "y.rs"}),
            &rows,
            &mut consumed,
            &by_sig,
        );
        assert_eq!(out, "two");
        assert!(!consumed[0]);
        assert!(consumed[1]);
    }

    #[test]
    fn take_edge_output_falls_back_to_callback_map() {
        let rows: Vec<Row> = vec![];
        let mut consumed = vec![];
        let mut by_sig = HashMap::new();
        let sig = tool_dedup_signature("grep", &json!({"pattern": "foo"}));
        by_sig.insert(sig, "from-map".into());
        let out = take_edge_output_for_tool_call(
            "grep",
            &json!({"pattern": "foo"}),
            &rows,
            &mut consumed,
            &by_sig,
        );
        assert_eq!(out, "from-map");
    }

    #[test]
    fn tool_calls_for_stall_guard_prefers_server_list() {
        let server = vec![json!({"id":"1","name":"bash","arguments":{}})];
        let edge = vec![Row {
            tool: "read_file".into(),
            args: json!({}),
            output: "".into(),
        }];
        let g = tool_calls_for_stall_guard(&server, &edge);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0]["name"], "bash");
    }

    #[test]
    fn tool_calls_for_stall_guard_synthetic_ids() {
        let edge = vec![
            Row {
                tool: "a".into(),
                args: json!({}),
                output: "".into(),
            },
            Row {
                tool: "b".into(),
                args: json!({"x":1}),
                output: "".into(),
            },
        ];
        let g = tool_calls_for_stall_guard(&[], &edge);
        assert_eq!(g[0]["id"], "edge-0");
        assert_eq!(g[1]["id"], "edge-1");
        assert_eq!(g[1]["name"], "b");
    }

    #[test]
    fn cacheable_tools_are_all_read_only() {
        const SIDE_EFFECTFUL: &[&str] = &[
            "bash",
            "write_file",
            "str_replace",
            "delete_file",
            "multi_edit",
            "git_commit",
            "git_stash",
            "git_checkout_file",
            "github_create_issue",
            "mo_query",
            "mo_snapshot",
            "mo_branch",
            "memory_store",
            "memory_purge",
            "memory_correct",
        ];
        for tool in CACHEABLE_TOOLS {
            assert!(
                !SIDE_EFFECTFUL.contains(tool),
                "CACHEABLE_TOOLS must not contain side-effectful tool: {tool}"
            );
        }
    }

    #[test]
    fn cacheable_tools_covers_git_and_github_reads() {
        for expected in &[
            "git_status",
            "git_diff",
            "git_log",
            "git_blame",
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "github_list_prs",
            "github_get_pr",
        ] {
            assert!(
                CACHEABLE_TOOLS.contains(expected),
                "missing cacheable tool: {expected}"
            );
        }
    }

    #[test]
    fn openai_assistant_message_from_server_tool_calls() {
        let server = vec![json!({
            "id": "call_1",
            "name": "read_file",
            "arguments": {"path": "a.rs"}
        })];
        let msg = openai_assistant_with_tool_calls_message(&server, &[] as &[Row], "");
        assert_eq!(msg["role"], "assistant");
        assert!(msg["content"].is_null());
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0]["id"], "call_1");
        assert_eq!(tc[0]["type"], "function");
        assert_eq!(tc[0]["function"]["name"], "read_file");
        let args: Value = serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap())
            .unwrap();
        assert_eq!(args["path"], "a.rs");
    }

    #[test]
    fn openai_assistant_message_from_edge_round_default_ids() {
        let edge = vec![Row {
            tool: "grep".into(),
            args: json!({"pattern": "x"}),
            output: "".into(),
        }];
        let msg = openai_assistant_with_tool_calls_message(&[], &edge, "");
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tc[0]["id"], "edge-0");
        assert_eq!(tc[0]["function"]["name"], "grep");
    }

    #[derive(Debug)]
    struct RowWithRequestId {
        tool: String,
        args: Value,
        output: String,
        request_id: String,
    }

    impl EdgeToolRoundRow for RowWithRequestId {
        fn tool_name(&self) -> &str {
            &self.tool
        }
        fn tool_args(&self) -> &Value {
            &self.args
        }
        fn tool_output(&self) -> &str {
            &self.output
        }
        fn assistant_tool_call_id(&self, index: usize) -> String {
            if self.request_id.is_empty() {
                format!("edge-{index}")
            } else {
                self.request_id.clone()
            }
        }
    }

    #[test]
    fn openai_assistant_message_edge_uses_request_id_when_set() {
        let edge = vec![RowWithRequestId {
            tool: "bash".into(),
            args: json!({"command": "true"}),
            output: "ok".into(),
            request_id: "req-abc".into(),
        }];
        let msg = openai_assistant_with_tool_calls_message(&[], &edge, "");
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tc[0]["id"], "req-abc");
    }

    #[test]
    fn openai_assistant_message_includes_reasoning_content_when_non_empty() {
        let msg = openai_assistant_with_tool_calls_message(
            &[],
            &[Row {
                tool: "t".into(),
                args: json!({}),
                output: "".into(),
            }],
            "think",
        );
        assert_eq!(msg["reasoning_content"], "think");
    }

    #[test]
    fn openai_tool_roundtrip_values_matches_headless_shape() {
        let (m, tr) = openai_tool_roundtrip_values("call-1", "read_file", "ok");
        assert_eq!(m["role"], "tool");
        assert_eq!(m["tool_call_id"], "call-1");
        assert_eq!(m["content"], "ok");
        assert_eq!(tr["tool_call_id"], "call-1");
        assert_eq!(tr["name"], "read_file");
        assert_eq!(tr["result"], "ok");
    }
}
