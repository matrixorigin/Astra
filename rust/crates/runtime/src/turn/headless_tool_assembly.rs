//! Match cloud assistant `tool_calls` to edge-executed `tool_request` rows (§5.5 headless path).
//!
//! Shared between the CLI SSE loop and any future server-side handler that consumes the same shape.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::tool_result_semantics::tool_dedup_signature;

/// One tool slot to execute in a headless round: either a server `tool_calls[i]` or synthetic edge row `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessRoundToolIdx {
    ServerToolCall(usize),
    SyntheticEdge(usize),
}

/// Resolved id/name/args for one headless round slot (server `tool_calls` row or synthetic edge row).
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessResolvedToolSlot {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub synthetic_edge_index: Option<usize>,
}

/// Map one [`HeadlessRoundToolIdx`] to flat call fields; `edge_lookup` is used only for [`HeadlessRoundToolIdx::SyntheticEdge`].
#[must_use]
pub fn resolve_headless_tool_slot(
    item: HeadlessRoundToolIdx,
    server_tool_calls: &[Value],
    mut edge_lookup: impl FnMut(usize) -> (String, Value),
) -> HeadlessResolvedToolSlot {
    match item {
        HeadlessRoundToolIdx::ServerToolCall(i) => {
            let (id, name, args) = server_tool_calls
                .get(i)
                .map(parse_flat_tool_call_event)
                .unwrap_or_else(|| (String::new(), String::new(), json!({})));
            HeadlessResolvedToolSlot {
                id,
                name,
                args,
                synthetic_edge_index: None,
            }
        }
        HeadlessRoundToolIdx::SyntheticEdge(i) => {
            let (name, args) = edge_lookup(i);
            HeadlessResolvedToolSlot {
                id: format!("edge-{i}"),
                name,
                args,
                synthetic_edge_index: Some(i),
            }
        }
    }
}

/// Prefer iterating server `tool_calls` when present; otherwise one synthetic slot per edge row (§5.5).
pub fn headless_round_tool_indices(
    server_tool_calls_len: usize,
    edge_round_len: usize,
) -> Vec<HeadlessRoundToolIdx> {
    if server_tool_calls_len > 0 {
        (0..server_tool_calls_len)
            .map(HeadlessRoundToolIdx::ServerToolCall)
            .collect()
    } else {
        (0..edge_round_len)
            .map(HeadlessRoundToolIdx::SyntheticEdge)
            .collect()
    }
}

/// Ensure every tool_call in the slice has a non-empty `"id"` field.
/// Returns a `Cow::Borrowed` when all ids are present, avoiding allocation.
/// When any id is empty/missing, clones those entries and patches them
/// with a synthetic UUID v7.
pub fn ensure_tool_call_ids(tool_calls: &[Value]) -> std::borrow::Cow<'_, [Value]> {
    let needs_patch = tool_calls.iter().any(|tc| {
        tc.get("id")
            .and_then(|v| v.as_str())
            .map_or(true, |s| s.is_empty())
    });
    if !needs_patch {
        return std::borrow::Cow::Borrowed(tool_calls);
    }
    std::borrow::Cow::Owned(
        tool_calls
            .iter()
            .map(|tc| {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    let mut patched = tc.clone();
                    patched["id"] = Value::String(uuid::Uuid::now_v7().to_string());
                    patched
                } else {
                    tc.clone()
                }
            })
            .collect(),
    )
}

/// Parse flat `/chat/turn` tool-call JSON: top-level `id`, `name`, `arguments` (object or JSON string).
pub fn parse_flat_tool_call_event(tc: &Value) -> (String, String, Value) {
    let id = tc
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let name = tc
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let args_raw = tc
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let args = match args_raw {
        Value::String(s) => {
            serde_json::from_str::<Value>(&s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        other => other,
    };
    (id, name, args)
}

/// Tool names still pending when step scheduling times out (abort tail of `indices`).
pub fn headless_timeout_aborted_tool_names(
    indices: &[HeadlessRoundToolIdx],
    completed_tool_results_len: usize,
    server_tool_calls: &[Value],
    mut synthetic_tool_name: impl FnMut(usize) -> String,
) -> Vec<String> {
    indices
        .iter()
        .skip(completed_tool_results_len)
        .map(|idx| match *idx {
            HeadlessRoundToolIdx::ServerToolCall(i) => server_tool_calls
                .get(i)
                .map(|tc| parse_flat_tool_call_event(tc).1)
                .unwrap_or_default(),
            HeadlessRoundToolIdx::SyntheticEdge(i) => synthetic_tool_name(i),
        })
        .collect()
}

/// User-facing error when the LLM names a tool not in the local registry.
pub fn unknown_local_tool_error_message(name: &str, valid_tool_names: &HashSet<String>) -> String {
    let mut names: Vec<_> = valid_tool_names.iter().cloned().collect();
    names.sort();
    format!("Unknown tool '{}'. Available: {}", name, names.join(", "))
}

/// User-visible `tool` message body when replaying an idempotent cache hit.
#[must_use]
pub fn idempotency_cache_hit_message(cached_output: &str) -> String {
    format!("(cached from earlier turn — identical call)\n{cached_output}")
}

/// `tool` / `tool_results` body when the same signature was already executed this headless round.
pub const HEADLESS_DUPLICATE_WITHIN_TURN_BODY: &str =
    "(duplicate call — result same as previous identical call this turn)";

#[must_use]
pub fn headless_openai_duplicate_within_turn_pair(
    tool_call_id: &str,
    tool_name: &str,
) -> (Value, Value) {
    openai_tool_roundtrip_values(tool_call_id, tool_name, HEADLESS_DUPLICATE_WITHIN_TURN_BODY)
}

#[must_use]
pub fn headless_idempotency_hit_openai_pair(
    tool_call_id: &str,
    tool_name: &str,
    cached_output: &str,
) -> (Value, Value) {
    let body = idempotency_cache_hit_message(cached_output);
    openai_tool_roundtrip_values(tool_call_id, tool_name, body.as_str())
}

#[must_use]
pub fn headless_unknown_local_tool_openai_pair(
    tool_call_id: &str,
    tool_name: &str,
    valid_tool_names: &HashSet<String>,
) -> (Value, Value) {
    let err = unknown_local_tool_error_message(tool_name, valid_tool_names);
    openai_tool_roundtrip_values(tool_call_id, tool_name, err.as_str())
}

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
    "symbol_search",
    "hover_info",
    "call_graph",
    "type_hierarchy",
    "dead_code",
    "extract_members",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "git_blame",
    "git_file_history",
    "git_contributors",
    "git_log_search",
    "github_list_prs",
    "github_get_pr",
    "github_ci_status",
    "github_list_issues",
    "github_get_issue",
    "github_repo_stats",
    "get_agent_info",
];

/// One edge-executed tool row in the current LLM round (ordering preserved vs `tool_calls`).
pub trait EdgeToolRoundRow {
    fn tool_name(&self) -> &str;
    fn tool_args(&self) -> &Value;
    fn tool_output(&self) -> &str;
    fn tool_duration_ms(&self) -> u64 {
        0
    }

    /// OpenAI `tool_calls[].id` when synthesizing from an edge-only round (§5.5).
    /// Default `edge-{index}`; rows with a server `request_id` should override.
    fn assistant_tool_call_id(&self, index: usize) -> String {
        format!("edge-{index}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedEdgeToolOutput {
    pub output: String,
    pub duration_ms: u64,
}

/// Take output for a server-emitted `tool_call` by matching dedup signature against the edge round.
pub fn take_edge_output_for_tool_call<T: EdgeToolRoundRow>(
    name: &str,
    args: &Value,
    round: &[T],
    consumed: &mut [bool],
    by_sig: &HashMap<String, String>,
) -> String {
    take_edge_output_for_tool_call_with_duration(name, args, round, consumed, by_sig).output
}

pub fn take_edge_output_for_tool_call_with_duration<T: EdgeToolRoundRow>(
    name: &str,
    args: &Value,
    round: &[T],
    consumed: &mut [bool],
    by_sig: &HashMap<String, String>,
) -> MatchedEdgeToolOutput {
    let sig = tool_dedup_signature(name, args);
    for (i, e) in round.iter().enumerate() {
        if consumed.get(i).copied().unwrap_or(true) {
            continue;
        }
        if tool_dedup_signature(e.tool_name(), e.tool_args()) == sig {
            consumed[i] = true;
            return MatchedEdgeToolOutput {
                output: e.tool_output().to_string(),
                duration_ms: e.tool_duration_ms(),
            };
        }
    }
    MatchedEdgeToolOutput {
        output: by_sig.get(&sig).cloned().unwrap_or_else(|| {
            format!(
                "Error: headless edge protocol — expected SSE `tool_request` before assistant `tool_call` for `{name}` (no matching edge execution in this turn)."
            )
        }),
        duration_ms: 0,
    }
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
    openai_assistant_with_tool_calls_message_ext(
        server_tool_calls,
        edge_round,
        reasoning_content,
        false,
    )
}

/// Extended variant that accepts `force_reasoning_field`.
///
/// When `force_reasoning_field` is true the `reasoning_content` key is always
/// present (empty string when `reasoning_content` is blank).  Thinking-enabled
/// models require this on every assistant message.
pub fn openai_assistant_with_tool_calls_message_ext<T: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[T],
    reasoning_content: &str,
    force_reasoning_field: bool,
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
    if !reasoning_content.is_empty() {
        if let Some(obj) = msg.as_object_mut() {
            obj.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning_content.to_string()),
            );
        }
    } else if force_reasoning_field && let Some(obj) = msg.as_object_mut() {
        obj.insert(
            "reasoning_content".to_string(),
            Value::String(String::new()),
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

/// Assistant `tool_calls` message + iteration indices for the headless round loop (§5.5).
#[derive(Debug, Clone)]
pub struct HeadlessRoundOpening {
    pub assistant_message: Value,
    pub indices: Vec<HeadlessRoundToolIdx>,
    pub tool_count: usize,
}

/// Build the assistant message and per-tool indices.
#[must_use]
pub fn begin_headless_tool_round_opening<Edge: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[Edge],
    reasoning_content: &str,
) -> HeadlessRoundOpening {
    begin_headless_tool_round_opening_ext(server_tool_calls, edge_round, reasoning_content, false)
}

/// Extended variant that accepts `force_reasoning_field` for thinking-model sessions.
#[must_use]
pub fn begin_headless_tool_round_opening_ext<Edge: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[Edge],
    reasoning_content: &str,
    force_reasoning_field: bool,
) -> HeadlessRoundOpening {
    let assistant_message = openai_assistant_with_tool_calls_message_ext(
        server_tool_calls,
        edge_round,
        reasoning_content,
        force_reasoning_field,
    );
    let indices = headless_round_tool_indices(server_tool_calls.len(), edge_round.len());
    let tool_count = indices.len().max(1);
    HeadlessRoundOpening {
        assistant_message,
        indices,
        tool_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Row {
        tool: String,
        args: Value,
        output: String,
        duration_ms: u64,
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
        fn tool_duration_ms(&self) -> u64 {
            self.duration_ms
        }
    }

    #[test]
    fn take_edge_output_matches_first_unconsumed_row() {
        let rows = vec![
            Row {
                tool: "read_file".into(),
                args: json!({"path": "x.rs"}),
                output: "one".into(),
                duration_ms: 7,
            },
            Row {
                tool: "read_file".into(),
                args: json!({"path": "y.rs"}),
                output: "two".into(),
                duration_ms: 13,
            },
        ];
        let mut consumed = vec![false; 2];
        let by_sig: HashMap<String, String> = HashMap::new();
        let out = take_edge_output_for_tool_call_with_duration(
            "read_file",
            &json!({"path": "y.rs"}),
            &rows,
            &mut consumed,
            &by_sig,
        );
        assert_eq!(out.output, "two");
        assert_eq!(out.duration_ms, 13);
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
        let out = take_edge_output_for_tool_call_with_duration(
            "grep",
            &json!({"pattern": "foo"}),
            &rows,
            &mut consumed,
            &by_sig,
        );
        assert_eq!(out.output, "from-map");
        assert_eq!(out.duration_ms, 0);
    }

    #[test]
    fn headless_round_indices_server_first() {
        let v = headless_round_tool_indices(2, 5);
        assert_eq!(
            v,
            vec![
                HeadlessRoundToolIdx::ServerToolCall(0),
                HeadlessRoundToolIdx::ServerToolCall(1),
            ]
        );
    }

    #[test]
    fn headless_round_indices_edge_when_no_server_calls() {
        let v = headless_round_tool_indices(0, 2);
        assert_eq!(
            v,
            vec![
                HeadlessRoundToolIdx::SyntheticEdge(0),
                HeadlessRoundToolIdx::SyntheticEdge(1),
            ]
        );
    }

    #[test]
    fn resolve_headless_slot_server_and_synthetic() {
        let server = vec![json!({"id":"a","name":"read_file","arguments":{}})];
        let s0 =
            resolve_headless_tool_slot(HeadlessRoundToolIdx::ServerToolCall(0), &server, |_| {
                panic!("edge lookup not used")
            });
        assert_eq!(s0.id, "a");
        assert_eq!(s0.name, "read_file");
        assert_eq!(s0.args, json!({}));
        assert!(s0.synthetic_edge_index.is_none());

        let s1 = resolve_headless_tool_slot(HeadlessRoundToolIdx::SyntheticEdge(2), &[], |i| {
            assert_eq!(i, 2);
            ("bash".into(), json!({"command":"ls"}))
        });
        assert_eq!(s1.id, "edge-2");
        assert_eq!(s1.name, "bash");
        assert_eq!(s1.args, json!({"command":"ls"}));
        assert_eq!(s1.synthetic_edge_index, Some(2));
    }

    #[test]
    fn parse_flat_tool_call_string_arguments() {
        let tc = json!({
            "id": "c1",
            "name": "bash",
            "arguments": "{\"command\":\"ls\"}"
        });
        let (id, name, args) = parse_flat_tool_call_event(&tc);
        assert_eq!(id, "c1");
        assert_eq!(name, "bash");
        assert_eq!(args, json!({"command":"ls"}));
    }

    #[test]
    fn parse_flat_tool_call_object_arguments() {
        let tc = json!({
            "id": "c2",
            "name": "grep",
            "arguments": {"pattern": "x"}
        });
        let (id, name, args) = parse_flat_tool_call_event(&tc);
        assert_eq!(id, "c2");
        assert_eq!(name, "grep");
        assert_eq!(args, json!({"pattern":"x"}));
    }

    #[test]
    fn headless_timeout_aborted_names_tail() {
        let idx = vec![
            HeadlessRoundToolIdx::ServerToolCall(0),
            HeadlessRoundToolIdx::SyntheticEdge(0),
        ];
        let server = vec![json!({"name":"read_file","arguments":{}})];
        let names = headless_timeout_aborted_tool_names(&idx, 1, &server, |_| "bash".to_string());
        assert_eq!(names, vec!["bash".to_string()]);
    }

    #[test]
    fn unknown_local_tool_lists_sorted() {
        let mut s = HashSet::new();
        s.insert("zebra".into());
        s.insert("alpha".into());
        let m = unknown_local_tool_error_message("foo", &s);
        assert_eq!(m, "Unknown tool 'foo'. Available: alpha, zebra");
    }

    #[test]
    fn idempotency_cache_hit_message_shape() {
        let m = idempotency_cache_hit_message("body");
        assert_eq!(m, "(cached from earlier turn — identical call)\nbody");
    }

    #[test]
    fn headless_duplicate_pair_matches_constant_body() {
        let (msg, tr) = headless_openai_duplicate_within_turn_pair("c1", "bash");
        assert_eq!(
            msg["content"].as_str(),
            Some(HEADLESS_DUPLICATE_WITHIN_TURN_BODY)
        );
        assert_eq!(
            tr["result"].as_str(),
            Some(HEADLESS_DUPLICATE_WITHIN_TURN_BODY)
        );
    }

    #[test]
    fn begin_headless_opening_counts_server_calls() {
        let server = vec![json!({"id":"1","name":"bash","arguments":{}})];
        let edge: Vec<Row> = vec![];
        let o = begin_headless_tool_round_opening(&server, &edge, "");
        assert_eq!(o.indices.len(), 1);
        assert_eq!(o.tool_count, 1);
    }

    #[test]
    fn tool_calls_for_stall_guard_prefers_server_list() {
        let server = vec![json!({"id":"1","name":"bash","arguments":{}})];
        let edge = vec![Row {
            tool: "read_file".into(),
            args: json!({}),
            output: "".into(),
            duration_ms: 0,
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
                duration_ms: 0,
            },
            Row {
                tool: "b".into(),
                args: json!({"x":1}),
                output: "".into(),
                duration_ms: 0,
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
        let args: Value =
            serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "a.rs");
    }

    #[test]
    fn openai_assistant_message_from_edge_round_default_ids() {
        let edge = vec![Row {
            tool: "grep".into(),
            args: json!({"pattern": "x"}),
            output: "".into(),
            duration_ms: 0,
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
                duration_ms: 0,
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

    #[test]
    fn cacheable_tools_includes_git_show() {
        assert!(
            CACHEABLE_TOOLS.contains(&"git_show"),
            "git_show should be cacheable (idempotent read of committed content)"
        );
    }

    #[test]
    fn parse_flat_tool_call_generates_id_when_missing() {
        let tc = json!({"name": "bash", "arguments": "{}"});
        let (id, name, _) = parse_flat_tool_call_event(&tc);
        assert!(!id.is_empty(), "empty id should be replaced with UUID");
        assert_eq!(name, "bash");

        // Empty string id should also be replaced
        let tc2 = json!({"id": "", "name": "bash", "arguments": "{}"});
        let (id2, _, _) = parse_flat_tool_call_event(&tc2);
        assert!(!id2.is_empty());
        assert_ne!(id, id2, "each call should get a unique id");
    }

    // ── ensure_tool_call_ids regression tests ───────────────────────────

    #[test]
    fn ensure_ids_borrows_when_all_present() {
        let tcs = vec![
            json!({"id": "a", "name": "bash"}),
            json!({"id": "b", "name": "grep"}),
        ];
        let result = ensure_tool_call_ids(&tcs);
        assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn ensure_ids_patches_empty_id() {
        let tcs = vec![
            json!({"id": "", "name": "bash"}),
            json!({"id": "ok", "name": "grep"}),
        ];
        let result = ensure_tool_call_ids(&tcs);
        assert!(matches!(result, std::borrow::Cow::Owned(_)));
        let id0 = result[0]["id"].as_str().unwrap();
        assert!(!id0.is_empty(), "empty id must be patched");
        assert_eq!(result[1]["id"].as_str().unwrap(), "ok", "valid id untouched");
    }

    #[test]
    fn ensure_ids_patches_missing_id() {
        let tcs = vec![json!({"name": "bash"})];
        let result = ensure_tool_call_ids(&tcs);
        let id = result[0]["id"].as_str().unwrap();
        assert!(!id.is_empty());
    }

    #[test]
    fn ensure_ids_unique_per_call() {
        let tcs = vec![
            json!({"id": "", "name": "a"}),
            json!({"id": "", "name": "b"}),
        ];
        let result = ensure_tool_call_ids(&tcs);
        let id0 = result[0]["id"].as_str().unwrap();
        let id1 = result[1]["id"].as_str().unwrap();
        assert_ne!(id0, id1, "each empty id must get a distinct UUID");
    }

    /// The critical invariant: after ensure_tool_call_ids, building an
    /// assistant message and parsing tool result ids must produce matching ids.
    #[test]
    fn ensure_ids_makes_assistant_and_result_ids_match() {
        let tcs = vec![json!({"id": "", "name": "bash", "arguments": "{}"})];
        let patched = ensure_tool_call_ids(&tcs);

        // Assistant message path
        let assistant_msg = openai_assistant_with_tool_calls_message::<Row>(
            &patched, &[], "",
        );
        let assistant_id = assistant_msg["tool_calls"][0]["id"].as_str().unwrap();

        // Tool result path
        let (result_id, _, _) = parse_flat_tool_call_event(&patched[0]);

        assert_eq!(assistant_id, result_id,
            "assistant tool_call id and tool result id must match after ensure_tool_call_ids");
    }
}
