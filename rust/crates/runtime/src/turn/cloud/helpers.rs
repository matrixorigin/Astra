//! Shared helpers for the cloud compaction pipeline.
//!
//! These free functions are used across multiple compaction layers and
//! the compaction engine. They operate on `Message` values from
//! `astra_turn_core::compression_types`.

use astra_turn_core::compression_types::Message;
use std::collections::HashMap;
use std::ops::Range;

/// Index of the first message after the protected head.
///
/// Protected head = first system message + first user message.
/// This is the minimum block preserved by all compaction layers.
/// Returns 0 when the system message is absent or 1 when there's no
/// plausible first user message — the caller should handle the
/// empty-vector edge case.
pub fn protected_head_end(messages: &[Message]) -> usize {
    let sys_count = messages.iter().take_while(|m| m.role == "system").count();
    if messages.is_empty() {
        return 0;
    }
    messages[sys_count..]
        .iter()
        .position(|m| m.role == "user")
        .map(|i| sys_count + i + 1)
        .unwrap_or(sys_count)
}

/// Compute affected turn indices (0-based) for a range of message indices.
/// Collect unique turn indices from a message range.
///
/// Uses each message's `round_index` when available; otherwise falls back to
/// the positional heuristic `idx / 2`. The fallback is correct for strict
/// [user, assistant] turn-pair sessions; agentic sessions with interleaved
/// tool messages must carry `round_index` on every message for accuracy.
pub fn affected_turn_indices(messages: &[Message], msg_range: Range<usize>) -> Vec<u32> {
    if msg_range.is_empty() {
        return Vec::new();
    }
    if msg_range.end <= messages.len() {
        // Use message round_index when available, fallback to idx/2.
        let mut indices: Vec<u32> = msg_range
            .filter_map(|idx| messages[idx].round_index.or(Some((idx / 2) as u32)))
            .collect();
        indices.sort_unstable();
        indices.dedup();
        return indices;
    }
    // Range extends beyond available messages — fall back to pure positional heuristic.
    let start_turn = msg_range.start / 2;
    let end_turn = (msg_range.end.saturating_sub(1)) / 2;
    let mut indices: Vec<u32> = (start_turn..=end_turn)
        .filter_map(|t| t.try_into().ok())
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

// ─────────────────────────── CompactPlan ─────────────────────────────────
/// ReactiveCompact.
///
/// Both layers follow the same algorithm skeleton: compute the protected
/// head, reserve a tail, optionally find a user-message pivot in the drop
/// range, then splice in a boundary marker.  This struct captures the
/// computed values so each layer only needs to build its own boundary
/// `Message` and call [`apply_compact_plan`].
#[derive(Debug)]
pub struct CompactPlan {
    /// First message index after the protected head (system + first user).
    pub head_end: usize,
    /// First message index of the preserved tail.
    pub tail_start: usize,
    /// Index of a user-message pivot to preserve (None = no pivot).
    pub pivot_idx: Option<usize>,
    /// Number of messages actually removed (excludes the pivot).
    pub removed_count: usize,
    /// Sorted, deduplicated turn indices affected by the removal.
    pub affected_turns: Vec<u32>,
    /// Number of turns removed.
    pub turns_removed: usize,
    /// Estimated token count freed by the removal.
    pub freed_tokens: u64,
    /// User queries from the drop range (first N chars each, for summary).
    pub user_queries: Vec<String>,
}

/// Fixup: extend `tail_start` backward to prevent splitting assistant↔tool pairs.
///
/// If a tool-result message in the tail (index ≥ tail_start) references a
/// `tool_call_id` whose producing assistant with matching `tool_calls.id`
/// lies in the drop range (`head_end..tail_start`), the assistant must be
/// included in the tail. Otherwise the provider (Anthropic, OpenAI) rejects
/// the orphaned `tool_result` that has no matching tool-call in any
/// surviving message.
///
/// Runs to fixpoint since extending the boundary can expose new split pairs.
fn fix_tool_pair_boundary(messages: &[Message], head_end: usize, tail_start: usize) -> usize {
    // Build a map from tool_call_id → producer (assistant) index in one pass
    // over the drop range. O(n) instead of the O(n²) nested loop.
    let mut producer_map: HashMap<&str, usize> = HashMap::with_capacity(messages.len() - head_end);
    for (i, msg) in messages.iter().enumerate().skip(head_end) {
        if let Some(ref calls) = msg.tool_calls {
            for tc in calls {
                if !tc.id.is_empty() {
                    producer_map.entry(tc.id.as_str()).or_insert(i);
                }
            }
        }
    }

    let mut ts = tail_start;
    loop {
        let mut changed = false;
        // Scan forward from ts; when a tool call's producer is before ts,
        // shrink ts and restart the scan.
        for msg in messages.iter().skip(ts) {
            let Some(ref tcid) = msg.tool_call_id else {
                continue;
            };
            if let Some(&producer_idx) = producer_map.get(tcid.as_str()) {
                if producer_idx < ts {
                    ts = producer_idx;
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }
    ts
}

/// Compute the shared compaction plan.
///
/// Returns `None` when no messages need removal (`tail_start <= head_end` or
/// zero messages to drop after factoring out the pivot).
pub fn compute_compact_plan(
    messages: &[Message],
    keep_tail: usize,
    pivot_check: impl Fn(&Message) -> bool,
) -> Option<CompactPlan> {
    let head_end = protected_head_end(messages);
    let mut tail_start = messages.len().saturating_sub(keep_tail);

    if tail_start <= head_end {
        return None;
    }

    // Fixup: extend tail backward to avoid splitting assistant↔tool pairs.
    // If a tool result in the tail references a tool_call that only exists
    // in the drop range, the provider will reject the orphaned tool result.
    tail_start = fix_tool_pair_boundary(messages, head_end, tail_start);

    let tail_starts_with_pivot = messages.get(tail_start).is_some_and(&pivot_check);
    let pivot_idx = if tail_starts_with_pivot {
        None
    } else {
        messages[head_end..tail_start]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, m)| pivot_check(m))
            .map(|(i, _)| head_end + i)
    };

    let removed_count = match pivot_idx {
        Some(_) => (tail_start - head_end) - 1,
        None => tail_start - head_end,
    };
    if removed_count == 0 {
        return None;
    }

    let affected_turns = match pivot_idx {
        Some(p) => {
            let pivot_turn = messages[p].round_index.unwrap_or((p / 2) as u32);
            let mut t = affected_turn_indices(messages, head_end..p);
            t.extend(affected_turn_indices(messages, p + 1..tail_start));
            t.retain(|&tidx| tidx != pivot_turn);
            t.sort_unstable();
            t.dedup();
            t
        }
        None => affected_turn_indices(messages, head_end..tail_start),
    };
    let turns_removed = affected_turns.len();

    let drop_range = head_end..tail_start;
    // Single traversal over the drop range: compute freed_tokens AND collect
    // short user-query previews for the boundary summary.
    let mut user_queries: Vec<String> = Vec::new();
    let freed_tokens: u64 = messages[drop_range.clone()]
        .iter()
        .enumerate()
        .filter(|(i, _)| pivot_idx.map_or(true, |p| *i + head_end != p))
        .map(|(_idx, m)| {
            // Collect user queries trimmed to 100 chars for the summarised boundary.
            if m.role == "user" {
                if let Some(ref content) = m.content {
                    let snippet = if content.len() > 100 {
                        format!("{}…", &content[..content.floor_char_boundary(100)])
                    } else {
                        content.clone()
                    };
                    user_queries.push(snippet);
                }
            }
            let content_tokens = m
                .content
                .as_deref()
                .map(crate::prompts::estimate_str_tokens)
                .unwrap_or(0);
            let tc_tokens = m
                .tool_calls
                .as_ref()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|tc| crate::prompts::estimate_str_tokens(&tc.function.arguments) + 15)
                        .sum::<usize>()
                })
                .unwrap_or(0);
            (content_tokens + tc_tokens + crate::prompts::PER_MESSAGE_OVERHEAD) as u64
        })
        .sum();

    Some(CompactPlan {
        head_end,
        tail_start,
        pivot_idx,
        removed_count,
        affected_turns,
        turns_removed,
        freed_tokens,
        user_queries,
    })
}

/// Splice messages according to a [`CompactPlan`], inserting `boundary` at the
/// cut point.
///
/// When a pivot is present, this drops `[head_end..pivot)` and
/// `[pivot+1..tail_start)` in two high-to-low splices so the first splice
/// doesn't shift the pivot index used by the second.
pub fn apply_compact_plan(messages: &mut Vec<Message>, plan: &CompactPlan, boundary: Message) {
    match plan.pivot_idx {
        Some(p) => {
            debug_assert!(
                plan.head_end <= p && p < plan.tail_start,
                "splice ordering invariant broken: head_end={}, p={p}, tail_start={}",
                plan.head_end,
                plan.tail_start
            );
            if plan.head_end <= p && p < plan.tail_start {
                messages.splice(p + 1..plan.tail_start, std::iter::empty());
                messages.splice(plan.head_end..p, std::iter::once(boundary));
            } else {
                // Fallback: invariant violated, drop the whole range.
                messages.splice(plan.head_end..plan.tail_start, std::iter::once(boundary));
            }
        }
        None => {
            messages.splice(plan.head_end..plan.tail_start, std::iter::once(boundary));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── protected_head_end ──────────────────────────────────────────────

    #[test]
    fn test_protected_head_end() {
        let cases: &[(&[(&str, &str)], usize)] = &[
            (&[], 0),
            (&[("system", "S")], 1),
            (&[("system", "S"), ("user", "U1")], 2),
            (&[("system", "S1"), ("system", "S2"), ("user", "U1")], 3),
            (
                &[
                    ("system", "S"),
                    ("user", "U1"),
                    ("assistant", "A1"),
                    ("user", "U2"),
                    ("assistant", "A2"),
                ],
                2,
            ),
            (&[("user", "U1"), ("assistant", "A1")], 1),
        ];
        for (input, expected) in cases {
            let msgs: Vec<Message> = input.iter().map(|(r, c)| msg(r, c, None)).collect();
            assert_eq!(protected_head_end(&msgs), *expected, "input: {input:?}");
        }
    }

    // ── affected_turn_indices ──────────────────────────────────────────

    #[test]
    fn test_affected_turn_indices() {
        // (messages_len, range_start, range_end) → expected turns
        let cases: &[(usize, usize, usize, &[u32])] = &[
            (0, 0, 1, &[0]),
            (0, 0, 2, &[0]),
            (0, 1, 3, &[0, 1]),
            (0, 5, 5, &[]),
            (0, 4, 10, &[2, 3, 4]),
            (0, 0, 4, &[0, 1]),
        ];
        for (msgs_len, start, end, expected) in cases {
            let msgs: Vec<Message> = (0..*msgs_len).map(|_| msg("user", "x", None)).collect();
            assert_eq!(
                affected_turn_indices(&msgs, *start..*end).as_slice(),
                *expected,
                "msgs_len={msgs_len}, range={start}..{end}"
            );
        }
    }

    #[test]
    fn affected_turns_uses_round_index_when_present() {
        let mut msgs = vec![
            msg("user", "U0", None),
            msg("assistant", "A0", None),
            msg("tool", "R0a", Some("c0")),
            msg("tool", "R0b", Some("c1")),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("tool", "R1a", Some("c2")),
        ];
        // Set round_index: messages at idx 0-3 are turn 0, idx 4-6 are turn 1
        for (i, m) in msgs.iter_mut().enumerate() {
            m.round_index = Some((i / 4) as u32); // 0..4 → 0, 4..7 → 1
        }
        // Without round_index, (idx/2) would give [0,0,1,1,2,2,3] → [0,1,2,3]
        // With round_index, we get [0,0,0,0,1,1,1] → [0,1]
        assert_eq!(affected_turn_indices(&msgs, 0..7), vec![0, 1]);
    }

    // ── fix_tool_pair_boundary ──────────────────────────────────────

    #[test]
    fn fix_tool_pair_no_split_needed() {
        // assistant(c1) and tool(c1) are both in drop range.
        // tail starts after both → no split, no change.
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            assistant_with_tool_calls(&[("c1", "read", "{}")]),
            msg("tool", "result1", Some("c1")),
            msg("user", "U2", None),
            msg("assistant", "A2", None),
        ];
        // head_end=2, tail_start=4 (keep 2 tail: U2, A2)
        // asst(c1)@2 + tool(c1)@3 are both in [2..4) → no orphan
        assert_eq!(fix_tool_pair_boundary(&msgs, 2, 4), 4);
    }

    #[test]
    fn fix_tool_pair_retreats_for_orphan_tool_result() {
        // assistant(c1)@3 in drop range, tool(c1)@5 in tail → orphan.
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            assistant_with_tool_calls(&[("c1", "read", "{}")]), // [2]
            msg("user", "U2", None),                            // [3]
            msg("assistant", "A2", None),                       // [4]
            msg("tool", "result1", Some("c1")),                 // [5] orphan if tail_start=5
            msg("assistant", "A3", None),                       // [6]
        ];
        // tail_start=5 → tool(c1)@5 is orphaned (producer asst(c1)@2)
        // fix should retreat tail_start to 2
        assert_eq!(fix_tool_pair_boundary(&msgs, 2, 5), 2);
    }

    #[test]
    fn fix_tool_pair_fixpoint_retreats_once_to_producer() {
        // assistant(c1)@2 in drop, tool(c1)@3 in drop, assistant(c2)@4 in drop,
        // tool(c2)@5 in tail. Fix retreats to asst(c2)@4 (one level, not all the
        // way to 2 — because after retreat, tool(c1) is in the drop range together
        // with its producer asst(c1), so no further retreat is needed).
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            assistant_with_tool_calls(&[("c1", "grep", "{}")]), // [2]
            msg("tool", "result1", Some("c1")),                 // [3]
            assistant_with_tool_calls(&[("c2", "read", "{}")]), // [4]
            msg("tool", "result2", Some("c2")),                 // [5]
            msg("user", "U3", None),                            // [6]
        ];
        // tail_start=5: tail=[tool(c2), U3], tool(c2) is orphaned
        // → retreat to asst(c2) producer at index 4
        assert_eq!(fix_tool_pair_boundary(&msgs, 2, 5), 4);
    }

    #[test]
    fn fix_tool_pair_no_tool_results_in_tail() {
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("assistant", "A2", None),
            msg("user", "U3", None),
            msg("assistant", "A4", None),
        ];
        // No tool_call_id in tail — no changes needed
        assert_eq!(fix_tool_pair_boundary(&msgs, 2, 4), 4);
    }

    // ── compute_compact_plan ───────────────────────────────────────────

    #[test]
    fn plan_noop_when_tail_within_head() {
        // head_end = index 2 (system + first user), keep_tail = 4, total msgs = 4
        // → tail_start = 0 → tail_start <= head_end → None.
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("user", "U2", None),
        ];
        let plan = compute_compact_plan(&msgs, 4, |m| m.is_plain_user_task());
        assert!(plan.is_none(), "tail within protected head → noop");
    }

    #[test]
    fn plan_noop_when_no_messages_to_drop() {
        // head_end = 2, keep_tail = 2, total = 4 → tail_start = 2 → removed = 0.
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("user", "U2", None),
        ];
        let plan = compute_compact_plan(&msgs, 2, |m| m.is_plain_user_task());
        assert!(plan.is_none());
    }

    #[test]
    fn plan_basic_without_pivot() {
        // 8 messages (4 turns), keep_tail = 2 → tail_start = 6.
        // head_end = 2 → drop [2..6) = 4 msgs removed.
        // No pivot because tail_start (idx 6) is a real user.
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("assistant", "A2", None),
            msg("assistant", "A3", None),
            msg("assistant", "A4", None),
            msg("user", "U2", None), // tail_start (6): real user → no pivot
            msg("assistant", "A5", None),
        ];
        let plan = compute_compact_plan(&msgs, 2, |m| m.is_plain_user_task())
            .expect("should produce a plan");
        assert_eq!(plan.head_end, 2);
        assert_eq!(plan.tail_start, 6);
        assert!(plan.pivot_idx.is_none(), "tail_start msg is a real user");
        assert_eq!(plan.removed_count, 4);
        assert_eq!(plan.turns_removed, 2); // turns 1,2
    }

    #[test]
    fn plan_with_pivot_preservation() {
        // 8 messages, tail_start = 6. Tail msg at idx 6 is assistant,
        // but idx 4 is a real user → pivot at 4.
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("user", "U2", None),
            msg("assistant", "A2", None),
            msg("user", "U3", None),
            msg("assistant", "A3", None), // tail_start = 6: assistant
            msg("assistant", "A4", None), // keep_tail = 2 → preserved
        ];
        let plan = compute_compact_plan(&msgs, 2, |m| m.is_plain_user_task())
            .expect("should produce a plan");
        assert_eq!(plan.pivot_idx, Some(5)); // U3 at idx 5
        assert_eq!(plan.removed_count, 3); // 4 in range - 1 pivot
        assert_eq!(plan.affected_turns.len(), 1); // only turn 1 (U2/A2)
    }

    #[test]
    fn plan_empty_messages() {
        let plan = compute_compact_plan(&[], 2, |m| m.is_plain_user_task());
        assert!(plan.is_none());
    }

    #[test]
    fn plan_pivot_with_custom_check() {
        // Custom pivot check: any message with role "user" OR "tool".
        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("tool", "tool_result", Some("call_1")),
            msg("assistant", "A2", None),
            msg("assistant", "A3", None),
            msg("assistant", "A4", None),
            msg("user", "U4", None),
        ];
        let plan =
            compute_compact_plan(&msgs, 2, |m: &Message| m.role == "user" || m.role == "tool")
                .expect("should produce a plan");
        // tail_start = 6, msg at idx 6 is assistant → needs pivot.
        // Scan backward: idx 5 = A3 (no), idx 4 = A2 (no), idx 3 = tool (yes).
        assert_eq!(plan.pivot_idx, Some(3));
    }

    // ── compute_compact_plan: tool-result orphan prevention ──────────

    #[test]
    fn compute_compact_plan_does_not_orphan_tool_results() {
        // When tail_start splits an assistant(tool_calls:[cX]) from its
        // tool(tool_call_id:cX) result, the tail is extended backward
        // to include the producing assistant so providers don't reject
        // the orphaned tool_result.
        //
        // Layout (6 msgs after protected head: system+user at indices 0-1):
        //   [2] assistant("", [c1])     – produces tool call c1
        //   [3] tool("result", c1)      – tool result for c1
        //   [4] user("U2")              – plain user
        //   [5] assistant("", [c2])     – produces tool call c2
        //   [6] tool("result", c2)      – tool result for c2
        //   [7] user("U3")              – plain user
        //
        // keep_tail=2 → tail_start=8-2=6 → tail=[tool(c2), user(U3)]
        // tool(c2) is orphaned: producer asst(c2) at idx 5 < tail_start(6).
        // After fix: tail_start retreats to 5, drop_range=[2..5),
        // pivot=user(U2)@4, removed_count=(5-2)-1=2 (asst(c1)+tool(c1)).

        let msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None), // [1] head_end=2
            assistant_with_tool_calls(&[("c1", "read", "{}")]), // [2]
            msg("tool", "result1", Some("c1")), // [3]
            msg("user", "U2", None), // [4]
            assistant_with_tool_calls(&[("c2", "read", "{}")]), // [5]
            msg("tool", "result2", Some("c2")), // [6]
            msg("user", "U3", None), // [7]
        ];

        let plan = compute_compact_plan(&msgs, 2, |m| m.is_plain_user_task())
            .expect("should produce a plan");

        // tail_start must retreat from 6 to 5 to include asst(c2).
        assert_eq!(
            plan.tail_start, 5,
            "tail_start should be 5 (asst(c2)), got {}",
            plan.tail_start
        );

        // No message in the tail should be an orphaned tool result.
        for i in plan.tail_start..msgs.len() {
            if let Some(ref tcid) = msgs[i].tool_call_id {
                let has_producer = msgs[plan.tail_start..i].iter().any(|m| {
                    m.tool_calls
                        .as_ref()
                        .is_some_and(|calls| calls.iter().any(|tc| tc.id == *tcid))
                });
                assert!(
                    has_producer,
                    "orphan tool_result at idx {i} (tool_call_id={tcid}); tail_start={}",
                    plan.tail_start
                );
            }
        }
    }

    // ── apply_compact_plan ─────────────────────────────────────────────

    #[test]
    fn apply_plan_without_pivot() {
        let mut msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("assistant", "A2", None),
            msg("assistant", "A3", None),
            msg("assistant", "A4", None),
            msg("user", "U2", None), // tail_start: real user → no pivot
            msg("assistant", "A5", None),
        ];
        let plan = compute_compact_plan(&msgs, 2, |m| m.is_plain_user_task())
            .expect("should produce a plan");
        assert!(plan.pivot_idx.is_none());
        let boundary = msg("system", "[compacted]", None);
        apply_compact_plan(&mut msgs, &plan, boundary);
        // After: [S, U1, boundary, U2, A5]
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "system");
        assert_eq!(msgs[2].content.as_deref(), Some("[compacted]"));
        assert_eq!(msgs[3].role, "user");
        assert_eq!(msgs[3].content.as_deref(), Some("U2"));
    }

    #[test]
    fn apply_plan_with_pivot() {
        let mut msgs = vec![
            msg("system", "S", None),
            msg("user", "U1", None),
            msg("assistant", "A1", None),
            msg("assistant", "A2", None), // tail_start would normally be here
            msg("assistant", "A3", None),
            msg("user", "U4", None),      // real user pivot at idx 5
            msg("assistant", "A4", None), // preserved tail
            msg("user", "U5", None),      // preserved tail
        ];
        // keep_tail=2, head_end=2 → tail_start=6, pivot at 5.
        let plan = compute_compact_plan(&msgs, 2, |m| m.is_plain_user_task())
            .expect("should produce a plan");
        assert_eq!(plan.pivot_idx, Some(5));
        let boundary = msg("system", "[compacted]", None);
        apply_compact_plan(&mut msgs, &plan, boundary);
        // After: [system, U1, boundary, U4, A4, U5]
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[2].role, "system"); // boundary
        assert_eq!(msgs[3].role, "user"); // pivot (U4)
        assert_eq!(msgs[3].content.as_deref(), Some("U4"));
    }

    // ── helpers ─────────────────────────────────────────────────────────

    fn msg(role: &str, content: &str, tool_call_id: Option<&str>) -> Message {
        Message {
            role: role.into(),
            content: if content.is_empty() && tool_call_id.is_none() {
                None
            } else {
                Some(content.into())
            },
            content_is_tool_result: false,
            content_was_array: false,
            content_modified: false,
            is_synthetic: false,
            tool_calls: None,
            tool_call_id: tool_call_id.map(String::from),
            name: None,
            timestamp: None,
            round_index: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    fn assistant_with_tool_calls(calls: &[(&str, &str, &str)]) -> Message {
        use astra_turn_core::compression_types::{ToolCall, ToolCallFunction};
        Message {
            role: "assistant".into(),
            content: Some(String::new()),
            content_is_tool_result: false,
            content_was_array: false,
            content_modified: false,
            is_synthetic: false,
            tool_calls: Some(
                calls
                    .iter()
                    .map(|&(id, name, args)| ToolCall {
                        id: id.into(),
                        call_type: Some("function".into()),
                        function: ToolCallFunction {
                            name: name.into(),
                            arguments: args.into(),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
            name: None,
            timestamp: None,
            round_index: None,
            extra: std::collections::BTreeMap::new(),
        }
    }
}
