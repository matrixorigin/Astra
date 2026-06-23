//! Anthropic wire-level cache annotations (tool + message).
//!
//! These helpers place Anthropic `cache_control` markers on the
//! `tool_schemas[]` and `messages[]` arrays — the wire-level counterpart
//! to `cache_markers` on `system_blocks` (handled in `mod.rs`). They are
//! pure data transforms and live here (rather than in `astra-runtime`)
//! because the pipeline's Optimize+Serialize phase should be the single
//! owner of provider-specific wire annotations.
//!
//! Historical note: an earlier revision of this module also exported
//! `insert_cache_edits_block` / `annotate_tool_result_cache_references`
//! helpers that emitted `cache_edits` / `cache_reference` fields.
//! Those fields aren't part of Anthropic's public API; real
//! `/v1/messages` returns HTTP 400 on them (session 5c5cbf78,
//! 2026-05-08), so the helpers and all their state (session-keyed
//! pin map, merge logic) were removed entirely.

use serde_json::{Value, json};

/// Anthropic ephemeral cache-control marker — the wire value all four
/// annotation helpers place on schemas / messages / tool results.
#[must_use]
pub fn anthropic_ephemeral_cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

/// Place a single `cache_control` marker on the last always-load tool schema in
/// `tool_schemas`, ending the static-lib prefix. Later dynamic tools still
/// appear in the array but do not invalidate the cached prefix.
///
/// `always_load_names` is the set of tool names guaranteed present every turn
/// (the "static lib"). If no always-load tool is present (e.g. a delegated
/// sub-run with a fully custom toolset), the marker falls back to the last
/// tool in the array — cache hits on dynamic tail are still a best-effort
/// win, and the caller is expected to log a warning at the call-site.
///
/// No-op when `tool_schemas` is empty.
/// Annotates tool schemas with `cache_control` on the last always-load tool. See
/// module-level docs for cache-key rationale.
///
/// Fallback: if no always-load tools are present in the input slice, falls back to
/// marking the final tool to preserve cache-boundary behavior and emits a
/// `warn!` log (observability for the degraded case).
pub fn annotate_always_load_tool_schema(
    tool_schemas: &mut [Value],
    always_load_names: &std::collections::HashSet<String>,
) {
    if tool_schemas.is_empty() {
        return;
    }
    let marker_idx = match last_always_load_tool_index(tool_schemas, always_load_names) {
        Some(idx) => idx,
        None => {
            // Degraded path: no always-load tools present. Fall back to the last
            // tool to preserve the cache-boundary contract, but emit a warn!
            // so this shows up in observability (L3 remediation).
            tracing::warn!(
                target: "astra::cache",
                tool_count = tool_schemas.len(),
                always_load_count = always_load_names.len(),
                "annotate_always_load_tool_schema: no always-load tools found — \
                 falling back to final tool for cache_control marker"
            );
            tool_schemas.len() - 1
        }
    };
    tool_schemas[marker_idx]["cache_control"] = anthropic_ephemeral_cache_control();
}

fn last_always_load_tool_index(
    tools: &[Value],
    always_load_names: &std::collections::HashSet<String>,
) -> Option<usize> {
    if always_load_names.is_empty() {
        return None;
    }
    tools.iter().enumerate().rev().find_map(|(idx, t)| {
        let name = t
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)?;
        if always_load_names.contains(name) {
            Some(idx)
        } else {
            None
        }
    })
}

/// Place exactly one `cache_control` marker on the last non-system message.
///
/// This matches the reference agent's Anthropic/Bedrock request contract: system
/// blocks carry the stable-prefix markers, tools carry one marker at the end
/// of the always-load static prefix, and messages carry exactly one tail marker.
/// Historical messages stay byte-stable because we never rewrite an older
/// round to preserve a second "historical" marker.
///
/// **Budget:** Anthropic allows at most 4 `cache_control` breakpoints per
/// request. This function emits at most 1, leaving room for system + tools.
///
/// **Fallbacks:**
///   - all-system / empty input → no-op.
///   - trailing `role=system` messages are skipped; the marker lands on the
///     last non-system message before them.
pub fn annotate_last_message_cache_breakpoint(messages: &mut [Value]) {
    let Some(idx) = find_message_cache_breakpoint_target(messages) else {
        if !messages.is_empty() {
            tracing::warn!(
                target: "astra::cache",
                message_count = messages.len(),
                "annotate_last_message_cache_breakpoint: no non-system message found — skipping cache_control marker"
            );
        }
        return;
    };
    apply_cache_control_to_message(&mut messages[idx]);
}

/// Determine which message should receive the message-level cache breakpoint.
///
/// Trailing `role=system` messages are intentionally skipped because their
/// content changes every round (`Already Fetched`, working-set inventories,
/// coaching pings) and would otherwise churn the deepest cache boundary.
///
/// Unmarkable tail messages are also skipped: `apply_cache_control_to_message`
/// silently no-ops on tool messages with empty/object content or missing
/// `tool_call_id`, which would leave the request with **zero** message-level
/// markers. Walking back to the previous markable message preserves the
/// breakpoint budget instead.
fn find_message_cache_breakpoint_target(messages: &[Value]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| {
            let role_ok = m
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| role != "system");
            role_ok && is_markable(m)
        })
        .map(|(idx, _)| idx)
}

/// Whether `apply_cache_control_to_message` will actually attach a marker
/// to this message. Mirrors the runtime checks in that function so we never
/// pick a tail index that silently no-ops downstream.
fn is_markable(msg: &Value) -> bool {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    let content = msg.get("content");
    if role == "tool" {
        // Array form is safe — we attach to the last block when present.
        if let Some(arr) = content.and_then(Value::as_array) {
            return !arr.is_empty();
        }
        // String form requires a non-empty tool_call_id to synthesize a
        // tool_result block; without one we'd skip annotation.
        if content.is_some_and(Value::is_string) {
            return msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty());
        }
        // Any other shape (object, number, bool, null, missing) is dropped.
        return false;
    }
    // user/assistant: string content always works (we upgrade to a text
    // block); array content is fine if non-empty; anything else no-ops.
    if content.is_some_and(Value::is_string) {
        return true;
    }
    if let Some(arr) = content.and_then(Value::as_array) {
        return !arr.is_empty();
    }
    false
}

/// Apply `cache_control: ephemeral` to a single message.
///
/// For `user`/`assistant` messages with string content: upgrades to a
/// content-block array with one `{type: "text", cache_control: …}` entry.
/// For messages with array content: attaches to the last block.
/// For `tool` messages: upgrade string content to a `tool_result` content
/// block carrying `cache_control` inside content. Anthropic's API does
/// **not** accept message-level `cache_control` on tool messages, so we
/// always lift the marker into the content block.
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn apply_cache_control_to_message(msg: &mut Value) {
    let role = msg
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if role == "tool" {
        // Tool messages: wrap string content into a tool_result block with
        // content-level cache_control. If content is already an array, just
        // attach cache_control to the last block.
        //
        // Array path (content is already `[{type: "tool_result", ...}]`) is
        // safe unconditionally — we only touch `cache_control` on the last
        // block, never synthesize a `tool_use_id`.
        if let Some(arr) = msg.get_mut("content").and_then(Value::as_array_mut) {
            if let Some(last_block) = arr.last_mut() {
                last_block["cache_control"] = anthropic_ephemeral_cache_control();
            } else {
                tracing::warn!(
                    target: "astra::cache",
                    "apply_cache_control_to_message: tool message has empty content array — \
                     skipping cache_control"
                );
            }
            return;
        }
        // String path: we must synthesize a tool_result block, which requires
        // a non-empty `tool_use_id`. If the upstream message lacks one, skip
        // annotation rather than emit an invalid request (Anthropic rejects
        // `tool_use_id: ""` with 400). Losing the cache marker here is
        // strictly preferable to breaking the request.
        if msg.get("content").is_some_and(Value::is_string) {
            let tool_call_id = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if tool_call_id.is_empty() {
                tracing::warn!(
                    target: "astra::cache",
                    "apply_cache_control_to_message: tool message missing tool_call_id — \
                     skipping cache_control to avoid invalid tool_use_id"
                );
                return;
            }
            let text = msg["content"].as_str().unwrap_or_default().to_string();
            msg["content"] = json!([{
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": text,
                "cache_control": anthropic_ephemeral_cache_control(),
            }]);
            return;
        }
        tracing::warn!(
            target: "astra::cache",
            content_kind = msg.get("content").map(value_kind).unwrap_or("missing"),
            "apply_cache_control_to_message: tool message has unsupported content shape — \
             skipping cache_control"
        );
        return;
    }
    if msg.get("content").is_some_and(Value::is_string) {
        let text = msg["content"].as_str().unwrap_or_default().to_string();
        msg["content"] = json!([{
            "type": "text",
            "text": text,
            "cache_control": anthropic_ephemeral_cache_control(),
        }]);
    } else if let Some(arr) = msg.get_mut("content").and_then(Value::as_array_mut) {
        if let Some(last_block) = arr.last_mut() {
            last_block["cache_control"] = anthropic_ephemeral_cache_control();
        } else {
            tracing::warn!(
                target: "astra::cache",
                role = role.as_str(),
                "apply_cache_control_to_message: message has empty content array — skipping cache_control"
            );
        }
    } else {
        tracing::warn!(
            target: "astra::cache",
            role = role.as_str(),
            content_kind = msg.get("content").map(value_kind).unwrap_or("missing"),
            "apply_cache_control_to_message: message has unsupported content shape — skipping cache_control"
        );
    }
}

pub fn message_has_cache_control(msg: &Value) -> bool {
    // Top-level cache_control is preserved for forward compatibility: the
    // legacy runtime helper checked it before block-level, and any future
    // caller that places markers at the message level must still be
    // recognized here so `annotate_tool_result_cache_references` stops at
    // the right position.
    if msg.get("cache_control").is_some() {
        return true;
    }
    msg.get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("cache_control")
                    .map(|cc| !cc.is_null())
                    .unwrap_or(false)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": { "name": name, "description": "test tool" }
        })
    }

    #[test]
    fn annotate_always_load_tool_schema_marks_last_always_load() {
        let mut tools = vec![
            tool_schema("bash"),      // always-load
            tool_schema("read_file"), // always-load
            tool_schema("custom_a"),  // dynamic
            tool_schema("custom_b"),  // dynamic
        ];
        let always_load: std::collections::HashSet<String> =
            ["bash".into(), "read_file".into()].into_iter().collect();
        annotate_always_load_tool_schema(&mut tools, &always_load);
        // Marker goes on last always-load (read_file, idx 1), NOT the last
        // overall tool — dynamic churn after this marker is expected
        // and cache-safe.
        assert!(tools[0]["cache_control"].is_null());
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
        assert!(tools[2]["cache_control"].is_null());
        assert!(tools[3]["cache_control"].is_null());
    }

    #[test]
    fn annotate_always_load_tool_schema_fallback_on_last_when_no_always_load() {
        let mut tools = vec![tool_schema("custom_a"), tool_schema("custom_b")];
        let always_load = std::collections::HashSet::new();
        annotate_always_load_tool_schema(&mut tools, &always_load);
        assert!(tools[0]["cache_control"].is_null());
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn annotate_always_load_tool_schema_noop_on_empty() {
        let mut tools: Vec<Value> = Vec::new();
        annotate_always_load_tool_schema(&mut tools, &std::collections::HashSet::new());
        assert!(tools.is_empty());
    }

    #[test]
    fn cache_breakpoint_single_user_message_marks_it_directly() {
        // Turn 1: only one user message → fallback to marking the last
        // non-system message (same as legacy behaviour).
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        assert_eq!(msgs[0]["content"], "sys");
        let arr = msgs[1]["content"].as_array().expect("content is array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "hi");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn cache_breakpoint_multi_turn_marks_last_non_system_message() {
        // reference-agent semantics: the current tail message gets the sole
        // message-level marker.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "turn 1 question"}),
            json!({"role": "assistant", "content": "turn 1 answer"}),
            json!({"role": "user", "content": "turn 2 question"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        // system untouched
        assert_eq!(msgs[0]["content"], "sys");
        // historical messages untouched
        assert_eq!(msgs[1]["content"], "turn 1 question");
        assert_eq!(msgs[2]["content"], "turn 1 answer");
        let arr = msgs[3]["content"]
            .as_array()
            .expect("user content upgraded");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn cache_breakpoint_multi_turn_with_tool_results() {
        // Real pattern: [user1, assistant(tool_call), tool_result, user2, assistant2, user3]
        // Marker should go on user3 because it is the last non-system message.
        let mut msgs = vec![
            json!({"role": "user", "content": "turn 1"}),
            json!({"role": "assistant", "tool_calls": [{"id":"c1"}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "result"}),
            json!({"role": "user", "content": "turn 2"}),
            json!({"role": "assistant", "content": "answer 2"}),
            json!({"role": "user", "content": "turn 3"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let arr = msgs[5]["content"].as_array().expect("user3 upgraded");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
        // msgs[0..4] have no marker
        assert!(msgs[0]["content"].is_string());
        assert!(msgs[3]["content"].is_string());
        assert_eq!(msgs[4]["content"], "answer 2");
    }

    fn marker_indices(msgs: &[Value]) -> Vec<usize> {
        msgs.iter()
            .enumerate()
            .filter(|(_, m)| message_has_cache_control(m))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn cache_breakpoint_tracks_the_current_tail_across_turns() {
        // Simulate 3 turns. The sole message-level marker must advance with
        // the newest non-system message each round.
        let mut turn1 = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "q1"}),
        ];
        annotate_last_message_cache_breakpoint(&mut turn1);
        let t1 = marker_indices(&turn1);
        assert_eq!(t1, vec![1], "turn 1 marks the single user message");

        let mut turn2 = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "q2"}),
        ];
        annotate_last_message_cache_breakpoint(&mut turn2);
        let t2 = marker_indices(&turn2);
        assert_eq!(t2, vec![3], "turn 2 marks the current user tail");

        let mut turn3 = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "q2"}),
            json!({"role": "assistant", "content": "a2"}),
            json!({"role": "user", "content": "q3"}),
        ];
        annotate_last_message_cache_breakpoint(&mut turn3);
        let t3 = marker_indices(&turn3);
        assert_eq!(t3, vec![5], "turn 3 marks the current user tail");

        // Tail strictly advances each turn.
        assert!(t2[0] > t1[0]);
        assert!(t3[0] > t2[0]);
    }

    #[test]
    fn cache_breakpoint_keeps_historical_messages_unmodified() {
        // With a single tail marker, the previous round's messages remain
        // untouched in the next round.
        let system = json!({"role": "system", "content": "sys"});
        let user1 = json!({"role": "user", "content": "q1"});
        let asst1 = json!({"role": "assistant", "content": "a1"});
        let user2 = json!({"role": "user", "content": "q2"});

        let mut turn1_msgs = vec![system.clone(), user1.clone()];
        annotate_last_message_cache_breakpoint(&mut turn1_msgs);

        let mut turn2_msgs = vec![system.clone(), user1.clone(), asst1.clone(), user2.clone()];
        annotate_last_message_cache_breakpoint(&mut turn2_msgs);

        let marker2_idx = turn2_msgs
            .iter()
            .position(message_has_cache_control)
            .unwrap();
        assert_eq!(marker2_idx, 3, "marker sits on the latest user");
        assert_eq!(turn2_msgs[0], system);
        assert_eq!(turn2_msgs[1], user1);
        assert_eq!(turn2_msgs[2], asst1);
    }

    // ── tool-loop tail marker: observed in session d0640d3d ────────────
    //
    // reference-agent semantics still need to handle tool loops: when the
    // conversation tail is a tool_result / assistant-with-tool_calls, the
    // sole message-level marker must sit on that tail message.
    #[test]
    fn cache_breakpoint_tool_loop_tail_advances_past_last_user() {
        // Shape after 2 tool rounds within one user turn:
        //   [user, assistant(tc), tool, assistant(tc), tool]
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id":"c1", "function":{"name":"git"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id":"c2", "function":{"name":"git"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "r2"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let marks = marker_indices(&msgs);
        assert!(
            marks.contains(&4),
            "tail marker must sit on the last tool_result (idx 4) — \
             otherwise tool pairs appended inside the agentic loop are \
             re-sent uncached every round. got {marks:?}",
        );
        assert_eq!(marks.len(), 1, "tool loops still emit only one marker");
    }

    #[test]
    fn cache_breakpoint_tool_loop_tail_moves_with_newest_pair() {
        let mut round_n = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id":"c1", "function":{"name":"git"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id":"c2", "function":{"name":"git"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "r2"}),
        ];
        annotate_last_message_cache_breakpoint(&mut round_n);
        let n_marks = marker_indices(&round_n);
        let n_tail = *n_marks.last().expect("round N must emit a marker");
        assert_eq!(n_tail, 4, "round N tail is last tool_result");
        assert_eq!(n_marks.len(), 1, "round N emits one tail marker");

        let mut round_np1 = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id":"c1", "function":{"name":"git"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id":"c2", "function":{"name":"git"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "r2"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id":"c3", "function":{"name":"git"}}]}),
            json!({"role": "tool", "tool_call_id": "c3", "content": "r3"}),
        ];
        annotate_last_message_cache_breakpoint(&mut round_np1);
        let np1_marks = marker_indices(&round_np1);
        assert_eq!(np1_marks.len(), 1, "round N+1 emits one tail marker");
        let np1_tail = *np1_marks.last().unwrap();
        assert!(
            np1_tail > n_tail,
            "round N+1 tail must advance to the newly appended tool pair. got {np1_tail}",
        );
    }

    #[test]
    fn cache_breakpoint_marks_last_block_of_array_content() {
        let mut msgs = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ]
        })];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let arr = msgs[0]["content"].as_array().unwrap();
        assert!(arr[0]["cache_control"].is_null());
        assert_eq!(arr[1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn cache_breakpoint_marks_last_block_of_tool_array_content() {
        let mut msgs = vec![json!({
            "role": "tool",
            "tool_call_id": "tooluse_123",
            "content": [
                {"type": "tool_result", "tool_use_id": "tooluse_123", "content": "first"},
                {"type": "tool_result", "tool_use_id": "tooluse_123", "content": "second"},
            ]
        })];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let arr = msgs[0]["content"].as_array().unwrap();
        assert!(arr[0]["cache_control"].is_null());
        assert_eq!(arr[1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn cache_breakpoint_noop_on_system_only() {
        let mut msgs = vec![json!({"role": "system", "content": "sys"})];
        annotate_last_message_cache_breakpoint(&mut msgs);
        assert_eq!(msgs[0]["content"], "sys");
    }

    #[test]
    fn cache_breakpoint_tool_string_without_tool_call_id_stays_unmarked() {
        let mut msgs = vec![json!({
            "role": "tool",
            "content": "tool output"
        })];
        annotate_last_message_cache_breakpoint(&mut msgs);
        assert!(
            !message_has_cache_control(&msgs[0]),
            "tool messages without tool_call_id must stay unmarked rather than emit invalid tool_use_id"
        );
        assert_eq!(msgs[0]["content"], "tool output");
    }

    #[test]
    fn cache_breakpoint_tool_object_content_stays_unmarked() {
        let mut msgs = vec![json!({
            "role": "tool",
            "tool_call_id": "tooluse_123",
            "content": {"structured": true}
        })];
        annotate_last_message_cache_breakpoint(&mut msgs);
        assert!(
            !message_has_cache_control(&msgs[0]),
            "unsupported tool content shapes must stay unmarked instead of silently mutating"
        );
        assert_eq!(msgs[0]["content"], json!({"structured": true}));
    }

    #[test]
    fn cache_breakpoint_walks_back_past_unmarkable_tail() {
        // Tail is a tool message missing tool_call_id (unmarkable). The
        // marker must land on the previous markable message instead of
        // silently emitting zero markers — we still owe Anthropic exactly
        // one message-level breakpoint.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "a"}),
            json!({"role": "tool", "content": "result"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        assert!(
            !message_has_cache_control(&msgs[3]),
            "unmarkable tail stays untouched"
        );
        let arr = msgs[2]["content"]
            .as_array()
            .expect("assistant content upgraded to block array");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
    }

    /// Session c0905eab regression: runtime appends volatile signals
    /// (inventory, stall nudges) as
    /// `role=system` messages at the end of `state.messages`. Anthropic's
    /// wire converter merges these into the top-level `system[]` array.
    /// If `annotate_last_message_cache_breakpoint` lands its marker on
    /// one of those trailing system messages, the marker's content
    /// changes every round and invalidates the cache boundary for the
    /// rest of the request. Cache_read stays anchored at the size of the
    /// stable prefix only.
    ///
    /// The fix walks the tail backwards past any `role=system` msgs and
    /// anchors the marker on the last non-system msg.
    #[test]
    fn cache_breakpoint_tool_loop_skips_trailing_system_msgs() {
        // Shape after 2 tool rounds inside a user turn, THEN a trailing
        // volatile system msg appended by agentic_loop/lifecycle:
        //   [user, a(tc), tool, a(tc), tool, system(volatile)]
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id": "c2", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "r2"}),
            json!({"role": "system", "content": "## Already Fetched\n..."}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let marks = marker_indices(&msgs);
        assert!(
            !marks.contains(&5),
            "trailing system msg MUST NOT carry cache_control — its content \
             changes every round and would invalidate the cache boundary. \
             got {marks:?}",
        );
        assert!(
            marks.contains(&4),
            "fallback tail should be the last non-system msg (idx 4 = last \
             tool_result). got {marks:?}",
        );
    }

    /// Two trailing system msgs in a row (e.g. inventory + stall nudge).
    /// The walker should skip both and land on the last non-system msg.
    #[test]
    fn cache_breakpoint_tool_loop_skips_multiple_trailing_system_msgs() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r1"}),
            json!({"role": "system", "content": "⚠ stall nudge\n..."}),
            json!({"role": "system", "content": "## Already Fetched\n..."}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let marks = marker_indices(&msgs);
        for trailing in [3usize, 4] {
            assert!(
                !marks.contains(&trailing),
                "trailing system msg at idx {trailing} must not carry cc. got {marks:?}",
            );
        }
        assert!(
            marks.contains(&2),
            "fallback tail should be the last non-system msg (tool at idx 2). \
             got {marks:?}",
        );
    }

    #[test]
    fn cache_breakpoint_all_trailing_system_falls_through_to_normal_branch() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "q2"}),
            json!({"role": "system", "content": "## Already Fetched\n..."}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let marks = marker_indices(&msgs);
        // The trailing system at idx 3 must not be marked; the current user
        // tail at idx 2 should carry the sole marker.
        assert!(
            !marks.contains(&3),
            "trailing system must not be marked. got {marks:?}",
        );
        assert_eq!(marks, vec![2]);
    }
}
