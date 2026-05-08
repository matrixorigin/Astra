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

/// Place a single `cache_control` marker on the last pinned tool schema in
/// `tool_schemas`, ending the static-lib prefix. Later dynamic tools still
/// appear in the array but do not invalidate the cached prefix.
///
/// `pinned_names` is the set of tool names guaranteed present every turn
/// (the "static lib"). If no pinned tool is present (e.g. a delegated
/// sub-run with a fully custom toolset), the marker falls back to the last
/// tool in the array — cache hits on dynamic tail are still a best-effort
/// win, and the caller is expected to log a warning at the call-site.
///
/// No-op when `tool_schemas` is empty.
/// Annotates tool schemas with `cache_control` on the last pinned tool. See
/// module-level docs for cache-key rationale.
///
/// Fallback: if no pinned tools are present in the input slice, falls back to
/// marking the final tool to preserve cache-boundary behavior and emits a
/// `warn!` log (observability for the degraded case).
pub fn annotate_pinned_tool_schema(
    tool_schemas: &mut [Value],
    pinned_names: &std::collections::HashSet<String>,
) {
    if tool_schemas.is_empty() {
        return;
    }
    let marker_idx = match last_pinned_tool_index(tool_schemas, pinned_names) {
        Some(idx) => idx,
        None => {
            // Degraded path: no pinned tools present. Fall back to the last
            // tool to preserve the cache-boundary contract, but emit a warn!
            // so this shows up in observability (L3 remediation).
            tracing::warn!(
                target: "astra::cache",
                tool_count = tool_schemas.len(),
                pinned_count = pinned_names.len(),
                "annotate_pinned_tool_schema: no pinned tools found — \
                 falling back to final tool for cache_control marker"
            );
            tool_schemas.len() - 1
        }
    };
    tool_schemas[marker_idx]["cache_control"] = anthropic_ephemeral_cache_control();
}

fn last_pinned_tool_index(
    tools: &[Value],
    pinned_names: &std::collections::HashSet<String>,
) -> Option<usize> {
    if pinned_names.is_empty() {
        return None;
    }
    tools.iter().enumerate().rev().find_map(|(idx, t)| {
        let name = t
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)?;
        if pinned_names.contains(name) {
            Some(idx)
        } else {
            None
        }
    })
}

/// Place **rolling** `cache_control` markers that maximize prefix cache hits
/// across turns in a growing conversation.
///
/// **Strategy (rolling two breakpoints):** mark both
///   - the message just before the **second-to-last** user message (the
///     *historical* breakpoint — end of turn N-1), and
///   - the message just before the **last** user message (the *tail*
///     breakpoint — end of turn N).
///
/// This is the byte-identity invariant that makes Anthropic's prefix cache
/// work across consecutive LLM calls: turn N's *tail* index equals turn
/// N+1's *historical* index, so the messages up to that index serialize to
/// identical bytes in both turns. Rebuilding the marker from scratch every
/// round without this rolling scheme silently strips the previous round's
/// `cache_control` from older messages, invalidating the cached prefix
/// past `system + tools` — the regression captured in
/// `mock_llm_prompt_cache_e2e::rolling_breakpoint_*` tests.
///
/// **Budget:** Anthropic allows at most 4 `cache_control` breakpoints per
/// request. This function emits at most 2. The caller is responsible for
/// keeping the system+tools allocation to ≤ 2 markers so the total stays
/// within budget.
///
/// **Fallbacks:**
///   - 0 user messages → no-op.
///   - 1 user message (turn 1) → mark the last non-system message only
///     (no previous turn to carry forward).
///   - 2+ user messages but positions collapse (e.g. back-to-back users,
///     or historical would underflow to 0) → emit a single tail marker.
pub fn annotate_last_message_cache_breakpoint(messages: &mut [Value]) {
    if messages.is_empty() {
        return;
    }

    for idx in find_cache_breakpoint_targets(messages) {
        apply_cache_control_to_message(&mut messages[idx]);
    }
}

/// Determine which message indices should receive cache breakpoints.
///
/// Returns up to 2 indices in ascending order: `[historical, tail]` when
/// a rolling invariant is available, `[tail]` for single-turn /
/// incomplete-rolling, empty for none.
///
/// **Two rolling modes**, both obey the same cross-round invariant: *round
/// N's tail index == round N+1's historical index*. That byte-stable
/// anchor is what makes Anthropic's prefix cache hit through the
/// conversation history.
///
/// 1. **Normal multi-user turn growth** (chat UI pattern): conversation
///    ends on a `user` message and grows one `(assistant, user)` pair per
///    turn. tail = message before the last user; historical = message
///    before the penultimate user. Across turns, today's tail equals
///    tomorrow's historical by construction.
///
/// 2. **Tool-loop growth inside a single user turn** (agentic pattern,
///    observed in session d0640d3d causing 94% wasted cache_creation):
///    the conversation ends on a `tool` (tool_result) or `assistant`
///    (tool_call) message and grows one `(assistant_tc, tool_result)`
///    pair per LLM round. tail = `len - 1`; historical = `len - 3`
///    (two messages back — the previous round's tail). After appending
///    one more pair, new `len - 3` == old `len - 1` — the invariant.
fn find_cache_breakpoint_targets(messages: &[Value]) -> Vec<usize> {
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(i, _)| i)
        .collect();

    let last_non_system = || -> Option<usize> {
        messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, m)| {
                m.get("role")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r != "system")
            })
            .map(|(i, _)| i)
    };

    let is_non_system = |idx: usize| -> bool {
        messages
            .get(idx)
            .and_then(|m| m.get("role").and_then(Value::as_str))
            .is_some_and(|r| r != "system")
    };

    // Tool-loop detection: if there are any messages strictly after the
    // last user, the conversation tail is an assistant/tool, not a user.
    // Route through the tool-loop-aware branch so the tail advances with
    // each appended pair instead of staying pinned at `last_user - 1`.
    let last_user = user_indices.last().copied();
    if let Some(last_user) = last_user
        && messages.len() > last_user + 1
    {
        // Walk back through any trailing `role=system` msgs. The runtime
        // appends `[working-set:v1]`, `Already Fetched`, stall nudges etc.
        // as `role=system` entries at the end of `state.messages`
        // (agentic_loop_lifecycle.rs:787). Their content changes every
        // round, so putting a cache_control marker on one of them creates
        // an unstable cache boundary: the marker "wins" as the deepest
        // boundary (Anthropic merges role=system into the top-level
        // `system[]` array via `build_anthropic_system_and_messages`) and
        // invalidates every round.
        //
        // Session c0905eab t5 observed cached stuck at 2432 (system[0..2]
        // only) because the trailing working-set block carried cc and
        // overrode the stable system+tools cache. Controlled probe
        // recovered 7936 cached (31x) by skipping this marker. See
        // `tests/fixtures/deepseek_anthropic_cache_probe.py` callers.
        let mut tail_candidate = messages.len() - 1;
        while tail_candidate > last_user && !is_non_system(tail_candidate) {
            // Safe: `tail_candidate > last_user ≥ 0`.
            tail_candidate -= 1;
        }
        if tail_candidate == last_user {
            // All messages after the last user are `role=system`. No
            // meaningful non-system tail to mark; fall through to the
            // normal-turn branch (which will pick `last_user - 1`).
        } else {
            let tail = tail_candidate;
            // `tail - 2` is the previous round's tail (one `(assistant_tc,
            // tool)` pair back). Accept only when it's a real, non-system
            // index AND is strictly after `last_user` — if it would sit on
            // or before the last user we'd break the normal-turn invariant,
            // so fall through to just `[tail]`.
            let historical_candidate = tail.checked_sub(2);
            let historical =
                historical_candidate.filter(|&h| h > last_user && is_non_system(h) && h != tail);
            return match historical {
                Some(h) => vec![h, tail],
                None => vec![tail],
            };
        }
    }

    match user_indices.len() {
        0 => last_non_system().map(|i| vec![i]).unwrap_or_default(),
        1 => last_non_system().map(|i| vec![i]).unwrap_or_default(),
        _ => {
            let last_user = *user_indices.last().unwrap();
            let penult_user = user_indices[user_indices.len() - 2];

            // `tail` is the non-user message just before the last user turn
            // — the final assistant reply / tool result of the previous
            // turn. Guaranteed stable across the next round (that round
            // will see the exact same tail as its historical).
            let tail = last_user.saturating_sub(1);

            // Historical candidate: the message just before the penultimate
            // user turn. Valid only if it exists and is not a system
            // message — we never place message-level markers on system
            // (system blocks carry their own `cache_control` metadata via
            // `apply_cache_policy_to_blocks`).
            let historical = penult_user.checked_sub(1).filter(|&idx| is_non_system(idx));

            match historical {
                None => vec![tail],
                Some(h) if h == tail => vec![tail],
                Some(h) => vec![h, tail],
            }
        }
    }
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
fn apply_cache_control_to_message(msg: &mut Value) {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
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
        }
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
        }
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
    fn annotate_pinned_tool_schema_marks_last_pinned() {
        let mut tools = vec![
            tool_schema("bash"),      // pinned
            tool_schema("read_file"), // pinned
            tool_schema("custom_a"),  // dynamic
            tool_schema("custom_b"),  // dynamic
        ];
        let pinned: std::collections::HashSet<String> =
            ["bash".into(), "read_file".into()].into_iter().collect();
        annotate_pinned_tool_schema(&mut tools, &pinned);
        // Marker goes on last pinned (read_file, idx 1), NOT the last
        // overall tool — dynamic churn after this marker is expected
        // and cache-safe.
        assert!(tools[0]["cache_control"].is_null());
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
        assert!(tools[2]["cache_control"].is_null());
        assert!(tools[3]["cache_control"].is_null());
    }

    #[test]
    fn annotate_pinned_tool_schema_fallback_on_last_when_no_pinned() {
        let mut tools = vec![tool_schema("custom_a"), tool_schema("custom_b")];
        let pinned = std::collections::HashSet::new();
        annotate_pinned_tool_schema(&mut tools, &pinned);
        assert!(tools[0]["cache_control"].is_null());
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn annotate_pinned_tool_schema_noop_on_empty() {
        let mut tools: Vec<Value> = Vec::new();
        annotate_pinned_tool_schema(&mut tools, &std::collections::HashSet::new());
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
    fn cache_breakpoint_multi_turn_marks_before_last_user() {
        // Multi-turn: [user1, assistant1, user2] → marker on assistant1
        // (the last message from the previous turn). This makes prefix
        // [system + user1 + assistant1] stable across future turns.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "turn 1 question"}),
            json!({"role": "assistant", "content": "turn 1 answer"}),
            json!({"role": "user", "content": "turn 2 question"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        // system untouched
        assert_eq!(msgs[0]["content"], "sys");
        // user1 untouched
        assert_eq!(msgs[1]["content"], "turn 1 question");
        // assistant1 gets the marker (it's the message just before the last user)
        let arr = msgs[2]["content"]
            .as_array()
            .expect("assistant content upgraded");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
        // user2 (current turn) has NO marker
        assert_eq!(msgs[3]["content"], "turn 2 question");
    }

    #[test]
    fn cache_breakpoint_multi_turn_with_tool_results() {
        // Real pattern: [user1, assistant(tool_call), tool_result, user2, assistant2, user3]
        // Marker should go on assistant2 (message before user3 = last user).
        let mut msgs = vec![
            json!({"role": "user", "content": "turn 1"}),
            json!({"role": "assistant", "tool_calls": [{"id":"c1"}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "result"}),
            json!({"role": "user", "content": "turn 2"}),
            json!({"role": "assistant", "content": "answer 2"}),
            json!({"role": "user", "content": "turn 3"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        // Marker on msgs[4] (assistant "answer 2" — just before last user)
        let arr = msgs[4]["content"].as_array().expect("assistant2 upgraded");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
        // msgs[5] (current user) has no marker
        assert_eq!(msgs[5]["content"], "turn 3");
        // msgs[0..4] have no marker
        assert!(msgs[0]["content"].is_string());
        assert!(msgs[3]["content"].is_string());
    }

    fn marker_indices(msgs: &[Value]) -> Vec<usize> {
        msgs.iter()
            .enumerate()
            .filter(|(_, m)| message_has_cache_control(m))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn cache_breakpoint_rolls_forward_across_turns() {
        // Simulate 3 turns. The tail marker MUST advance each turn, and
        // from turn 3 onwards both historical and tail markers are
        // present — with historical equal to the previous turn's tail.
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
        // Penult user at idx 1, last user at idx 3. historical would be
        // idx 0 (system) — filtered out — so we get only the tail at idx 2.
        assert_eq!(
            t2,
            vec![2],
            "turn 2 emits tail only (historical = system filtered)"
        );

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
        // User indices: 1, 3, 5. penult=3, last=5. tail=4. historical=2.
        assert_eq!(
            t3,
            vec![2, 4],
            "turn 3 emits both historical and tail markers"
        );
        // Rolling invariant: turn 2's tail == turn 3's historical.
        assert_eq!(
            t2[0], t3[0],
            "turn 3's historical marker must sit at the same index as turn 2's tail",
        );

        // Tail strictly advances each turn.
        assert!(t3[t3.len() - 1] > t2[t2.len() - 1]);
    }

    #[test]
    fn cache_breakpoint_turn2_prefix_includes_turn1_entirely() {
        // The critical invariant: everything in turn 1's payload that was
        // BEFORE the marker in turn 1 must be IDENTICAL AND BEFORE the
        // marker in turn 2. This is what makes prefix caching work.
        let system = json!({"role": "system", "content": "sys"});
        let user1 = json!({"role": "user", "content": "q1"});
        let asst1 = json!({"role": "assistant", "content": "a1"});
        let user2 = json!({"role": "user", "content": "q2"});

        let mut turn1_msgs = vec![system.clone(), user1.clone()];
        annotate_last_message_cache_breakpoint(&mut turn1_msgs);

        let mut turn2_msgs = vec![system.clone(), user1.clone(), asst1.clone(), user2.clone()];
        annotate_last_message_cache_breakpoint(&mut turn2_msgs);

        // Turn 2's cacheable prefix: everything up to and including the marker.
        let marker2_idx = turn2_msgs
            .iter()
            .position(message_has_cache_control)
            .unwrap();
        // The prefix [0..=marker2_idx] must contain the exact same system + user1
        // that turn 1 had. The marker in turn 2 is on asst1 (index 2).
        assert_eq!(marker2_idx, 2, "marker on assistant = msg[2]");
        // system + user1 are byte-identical (they have no marker on them in turn 2)
        assert_eq!(turn2_msgs[0], system);
        assert_eq!(turn2_msgs[1], user1);
    }

    // ── tool-loop rolling: observed in session d0640d3d ────────────────
    //
    // Production capture showed the bridge re-issuing the same user turn
    // 14 times while appending `(assistant_tool_call, tool_result)` pairs
    // each round. With the pre-fix policy — tail = `last_user - 1` — the
    // marker froze at `msgs[last_user - 1]` regardless of how many tool
    // pairs were appended after `last_user`. New pairs sat AFTER the
    // marker → not in cached prefix → re-sent uncached every round.
    // Observed wasted cache_creation: ~44 K tokens over 14 rounds (94%).
    //
    // Post-fix contract: when the conversation tail is a
    // non-user message (tool_result / assistant-with-tool_calls — i.e.
    // inside an agentic tool loop), the tail marker MUST advance to the
    // last non-user message, so the next round's call hits cache through
    // every pair emitted so far.
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
    }

    // And the rolling invariant across tool rounds: round N's tail index
    // must equal round N+1's historical index, so Anthropic's prefix cache
    // hits through the previous round's tool_result.
    #[test]
    fn cache_breakpoint_tool_loop_rolls_forward_across_rounds() {
        // Round N: [u, a(tc), tool, a(tc), tool]  → markers should include idx 4
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

        // Round N+1: one more (assistant_tc, tool) pair appended.
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
        assert!(
            np1_marks.contains(&n_tail),
            "round N+1's historical marker must sit at round N's tail index \
             (= idx {n_tail}) so anthropic's prefix cache hits through that \
             point. round N markers={n_marks:?}, round N+1 markers={np1_marks:?}",
        );
        let np1_tail = *np1_marks.last().unwrap();
        assert!(
            np1_tail > n_tail,
            "round N+1 tail must advance past round N's (= {n_tail}) to \
             include the newly appended tool pair. got {np1_tail}",
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
    fn cache_breakpoint_noop_on_system_only() {
        let mut msgs = vec![json!({"role": "system", "content": "sys"})];
        annotate_last_message_cache_breakpoint(&mut msgs);
        assert_eq!(msgs[0]["content"], "sys");
    }

    /// Session c0905eab regression: runtime appends `[working-set:v1]`
    /// (and similar volatile signals — inventory, stall nudges) as
    /// `role=system` messages at the end of `state.messages`. Anthropic's
    /// wire converter merges these into the top-level `system[]` array.
    /// If `annotate_last_message_cache_breakpoint` lands its marker on
    /// one of those trailing system messages, the marker's content
    /// changes every round and invalidates the cache boundary for the
    /// rest of the request. Cache_read stays pinned at the size of the
    /// stable prefix only.
    ///
    /// The fix walks the tail backwards past any `role=system` msgs and
    /// anchors the marker on the last non-system msg (which is the one
    /// preserved byte-identical across rounds via the rolling-tail
    /// invariant).
    #[test]
    fn cache_breakpoint_tool_loop_skips_trailing_system_msgs() {
        // Shape after 2 tool rounds inside a user turn, THEN a trailing
        // working-set system msg appended by agentic_loop_lifecycle:
        //   [user, a(tc), tool, a(tc), tool, system(working-set)]
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id": "c2", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "r2"}),
            json!({"role": "system", "content": "[working-set:v1]\n..."}),
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

    /// Two trailing system msgs in a row (e.g. working-set + inventory).
    /// The walker should skip both and land on the last non-system msg.
    #[test]
    fn cache_breakpoint_tool_loop_skips_multiple_trailing_system_msgs() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r1"}),
            json!({"role": "system", "content": "[working-set:v1]\n..."}),
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

    /// When ALL messages after the last user are system msgs, no
    /// meaningful non-system tail exists in the tool-loop branch. Fall
    /// through to the normal branch which picks `last_user - 1`.
    #[test]
    fn cache_breakpoint_all_trailing_system_falls_through_to_normal_branch() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "q2"}),
            json!({"role": "system", "content": "[working-set:v1]\n..."}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let marks = marker_indices(&msgs);
        // Normal branch: tail = last_user - 1 = 1 (assistant "a1"). The
        // trailing system at idx 3 must not be marked.
        assert!(
            !marks.contains(&3),
            "trailing system must not be marked. got {marks:?}",
        );
    }
}
