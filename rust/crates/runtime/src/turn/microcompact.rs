//! Microcompact: clear old tool result content before each LLM call.
//!
//! Inspired by Claude Code's microcompact strategy. Tool results (file reads,
//! grep output, git diffs) dominate history token cost. Once the LLM has acted
//! on a tool result, the full content is rarely needed again. This module
//! replaces old tool result content with a short placeholder, keeping only the
//! most recent N results intact.
//!
//! Two triggers (whichever fires first):
//! - **Count-based**: more than `KEEP_RECENT` compactable results → clear oldest
//! - **Token-based**: total compactable content exceeds `TOKEN_BUDGET` → clear
//!   oldest until under budget (even if count ≤ KEEP_RECENT)
//!
//! This runs in-place on `state.messages` before each `execute_turn` call.

use serde_json::Value;

/// Placeholder that replaces cleared tool result content.
pub const CLEARED_PLACEHOLDER: &str = "[Previous tool output cleared]";

/// Marker for tool results persisted to disk by `tool_result_storage`.
/// These contain a file reference the LLM needs to re-read the output.
const PERSISTED_TAG: &str = "<persisted-output>";

/// Tool names whose results are safe to compact (read-only, reproducible).
/// Excluded: bash (non-idempotent), write_file/str_replace (mutation records),
/// skill (instructions needed for subsequent iterations).
const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file",
    "grep",
    "glob",
    "list_dir",
    "git_show",
    "git_diff",
    "git_log",
    "git_status",
    "web_search",
    "web_fetch",
];

/// How many recent compactable tool results to keep intact.
const KEEP_RECENT: usize = 6;

/// Maximum total estimated tokens for compactable tool results.
/// When exceeded, clear oldest results even if count ≤ KEEP_RECENT.
/// 12K tokens ≈ 48KB of content — enough for ~6 medium file reads.
const TOKEN_BUDGET: usize = 12_000;

/// Minimum content length (bytes) to bother compacting.
/// Short results cost few tokens and provide useful context.
const MIN_COMPACT_SIZE: usize = 500;

/// Rough token estimate for a string. ~4 bytes per token for English/code.
fn estimate_tokens(s: &str) -> usize {
    s.len() / 4
}

/// Compact old tool results in the message history.
///
/// Returns the number of tool results compacted and estimated tokens saved.
pub fn compact_tool_results(messages: &mut [Value], keep_recent: Option<usize>) -> CompactStats {
    let keep = keep_recent.unwrap_or(KEEP_RECENT);

    // Build tool_call_id → tool_name mapping from assistant messages.
    let mut id_to_name: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for msg in messages.iter() {
        if msg.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for tc in calls {
                    if let (Some(id), Some(name)) = (
                        tc.get("id").and_then(Value::as_str),
                        tc.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str),
                    ) {
                        id_to_name.insert(id, name);
                    }
                }
            }
        }
    }

    // Collect (index, content_tokens) of compactable tool result messages.
    let compactable: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| {
            if !is_compactable_tool_result(msg, &id_to_name) {
                return None;
            }
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            if content.len() < MIN_COMPACT_SIZE || content == CLEARED_PLACEHOLDER {
                return None;
            }
            Some((i, estimate_tokens(content)))
        })
        .collect();

    if compactable.is_empty() {
        return CompactStats::default();
    }

    // Determine how many to compact: max of count-based and token-based.
    let count_based = compactable.len().saturating_sub(keep);

    // Token-based: find the minimum number of oldest results to clear
    // so that the remaining total stays under TOKEN_BUDGET.
    let total_tokens: usize = compactable.iter().map(|(_, t)| t).sum();
    let token_based = if total_tokens > TOKEN_BUDGET {
        let mut cumulative = 0usize;
        let mut n = 0usize;
        for &(_, tokens) in &compactable {
            if total_tokens - cumulative <= TOKEN_BUDGET {
                break;
            }
            cumulative += tokens;
            n += 1;
        }
        // Always keep at least 1 result
        n.min(compactable.len() - 1)
    } else {
        0
    };

    let to_compact = count_based.max(token_based);
    let mut stats = CompactStats::default();

    for &(idx, tokens) in compactable.iter().take(to_compact) {
        stats.tokens_saved += tokens;
        stats.results_compacted += 1;
        messages[idx]["content"] = Value::String(CLEARED_PLACEHOLDER.to_string());
    }

    stats
}

/// Check if a message is a tool result from a compactable tool.
fn is_compactable_tool_result(
    msg: &Value,
    id_to_name: &std::collections::HashMap<&str, &str>,
) -> bool {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    if role != "tool" {
        return false;
    }
    // Skip persisted-to-disk results — they contain a file reference
    // the LLM needs to re-read the output.
    if let Some(content) = msg.get("content").and_then(Value::as_str) {
        if content.contains(PERSISTED_TAG) {
            return false;
        }
    }
    // Check tool name from the message itself
    if let Some(name) = msg.get("name").and_then(Value::as_str) {
        return COMPACTABLE_TOOLS.contains(&name);
    }
    // Look up tool name via tool_call_id → assistant message mapping
    if let Some(call_id) = msg.get("tool_call_id").and_then(Value::as_str) {
        if let Some(&name) = id_to_name.get(call_id) {
            return COMPACTABLE_TOOLS.contains(&name);
        }
    }
    // Unknown tool — don't compact (could be bash, skill, or write_file)
    false
}

#[derive(Debug, Default)]
pub struct CompactStats {
    pub results_compacted: usize,
    pub tokens_saved: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant_with_tools(calls: &[(&str, &str)]) -> Value {
        let tool_calls: Vec<Value> = calls
            .iter()
            .map(|(id, name)| json!({"id": id, "function": {"name": name}}))
            .collect();
        json!({"role": "assistant", "content": "", "tool_calls": tool_calls})
    }

    fn tool_result(id: &str, content: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": content})
    }

    // ── Count-based compaction ───────────────────────────────────────────

    #[test]
    fn compacts_old_results_keeps_recent() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            json!({"role": "user", "content": "review code"}),
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file"), ("c3", "grep")]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            assistant_with_tools(&[("c4", "read_file"), ("c5", "read_file")]),
            tool_result("c4", &big),
            tool_result("c5", &big),
        ];

        let stats = compact_tool_results(&mut messages, Some(2));

        assert_eq!(stats.results_compacted, 3);
        assert_eq!(messages[2]["content"], CLEARED_PLACEHOLDER);
        assert_eq!(messages[3]["content"], CLEARED_PLACEHOLDER);
        assert_eq!(messages[4]["content"], CLEARED_PLACEHOLDER);
        assert_eq!(messages[6]["content"], big); // recent kept
        assert_eq!(messages[7]["content"], big);
    }

    // ── Token-based compaction ───────────────────────────────────────────

    #[test]
    fn token_budget_triggers_even_under_keep_count() {
        // 3 results, each ~5K tokens = 15K total > TOKEN_BUDGET (12K).
        // keep=6, so count-based wouldn't trigger. Token-based should.
        let huge = "x".repeat(20_000); // ~5K tokens
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file"), ("c3", "read_file")]),
            tool_result("c1", &huge),
            tool_result("c2", &huge),
            tool_result("c3", &huge),
        ];

        let stats = compact_tool_results(&mut messages, Some(6));

        // Should compact at least 1 to get under budget
        assert!(
            stats.results_compacted >= 1,
            "token budget should trigger compaction even with count < keep, got {}",
            stats.results_compacted
        );
        // c3 (most recent) should be preserved
        assert_ne!(messages[3]["content"], CLEARED_PLACEHOLDER);
    }

    #[test]
    fn token_budget_always_keeps_at_least_one() {
        // 1 giant result that exceeds budget alone — token-based path
        // should still keep it (can't compact the only result).
        let giant = "x".repeat(100_000); // ~25K tokens >> TOKEN_BUDGET
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file")]),
            tool_result("c1", &giant),
        ];

        // With keep=6 (default), count-based won't trigger (1 < 6).
        // Token-based wants to clear, but min(n, len-1) = min(1, 0) = 0.
        let stats = compact_tool_results(&mut messages, None);

        assert_eq!(stats.results_compacted, 0);
        assert_ne!(messages[1]["content"], CLEARED_PLACEHOLDER);
    }

    // ── Safety: non-compactable tools ────────────────────────────────────

    #[test]
    fn skips_bash_skill_write_file() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tools(&[
                ("c1", "bash"),
                ("c2", "skill"),
                ("c3", "write_file"),
                ("c4", "str_replace"),
                ("c5", "delegate"),
                ("c6", "read_file"),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
            tool_result("c5", &big),
            tool_result("c6", &big),
        ];

        let stats = compact_tool_results(&mut messages, Some(0));

        assert_eq!(stats.results_compacted, 1); // only read_file
        assert_eq!(messages[1]["content"], big); // bash
        assert_eq!(messages[2]["content"], big); // skill
        assert_eq!(messages[3]["content"], big); // write_file
        assert_eq!(messages[4]["content"], big); // str_replace
        assert_eq!(messages[5]["content"], big); // delegate
        assert_eq!(messages[6]["content"], CLEARED_PLACEHOLDER); // read_file
    }

    #[test]
    fn skips_unknown_tool_call_ids() {
        let big = "x".repeat(1000);
        let mut messages = vec![tool_result("orphan", &big)];

        let stats = compact_tool_results(&mut messages, Some(0));
        assert_eq!(stats.results_compacted, 0);
        assert_eq!(messages[0]["content"], big);
    }

    // ── Safety: persisted-to-disk results ────────────────────────────────

    #[test]
    fn skips_persisted_to_disk_results() {
        let persisted = format!(
            "<persisted-output>\nTool `read_file` produced 50000 chars.\n\
             File: /tmp/sessions/tool_results/c1.txt\n\
             Preview: first 500 chars...\n</persisted-output>"
        );
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file")]),
            tool_result("c1", &persisted),
        ];

        let stats = compact_tool_results(&mut messages, Some(0));

        assert_eq!(stats.results_compacted, 0);
        assert!(messages[1]["content"].as_str().unwrap().contains("<persisted-output>"));
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn skips_short_results() {
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
            tool_result("c1", "short"),
            tool_result("c2", "also short"),
        ];

        let stats = compact_tool_results(&mut messages, Some(0));
        assert_eq!(stats.results_compacted, 0);
    }

    #[test]
    fn skips_non_tool_messages() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            json!({"role": "user", "content": &big}),
            json!({"role": "assistant", "content": &big}),
        ];

        let stats = compact_tool_results(&mut messages, Some(0));
        assert_eq!(stats.results_compacted, 0);
        assert_eq!(messages[0]["content"], big);
        assert_eq!(messages[1]["content"], big);
    }

    #[test]
    fn idempotent_on_already_compacted() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        compact_tool_results(&mut messages, Some(0));
        assert_eq!(messages[1]["content"], CLEARED_PLACEHOLDER);

        let stats = compact_tool_results(&mut messages, Some(0));
        assert_eq!(stats.results_compacted, 0); // already cleared
    }

    #[test]
    fn no_compaction_when_under_both_thresholds() {
        let small = "x".repeat(600); // > MIN_COMPACT_SIZE but small tokens
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
            tool_result("c1", &small),
            tool_result("c2", &small),
        ];

        let stats = compact_tool_results(&mut messages, Some(5));
        assert_eq!(stats.results_compacted, 0); // 2 results < keep=5, tokens < budget
    }

    // ── Realistic scenario: review session ───────────────────────────────

    #[test]
    fn realistic_review_session_compaction() {
        // Simulate session 746b6423: skill + 15 file reads across 2 iterations.
        // Iteration 1: skill(review-changes) + 11 read_file + 4 grep
        // Iteration 2: 3 more read_file
        // Before iteration 2, microcompact should clear old results.

        let file_content = "fn main() {\n".repeat(80); // ~960 bytes, realistic file
        let grep_output = "src/main.rs:10: fn main()\nsrc/lib.rs:5: pub fn run()\n".repeat(10);
        let skill_output = "# Review\nLooks good.\n".repeat(50); // ~1050 bytes

        let mut messages = vec![
            json!({"role": "user", "content": "review latest commit"}),
            // Iteration 1: assistant calls skill + tools
            assistant_with_tools(&[
                ("s1", "skill"),
                ("r1", "read_file"), ("r2", "read_file"), ("r3", "read_file"),
                ("r4", "read_file"), ("r5", "read_file"), ("r6", "read_file"),
                ("r7", "read_file"), ("r8", "read_file"), ("r9", "read_file"),
                ("r10", "read_file"), ("r11", "read_file"),
                ("g1", "grep"), ("g2", "grep"), ("g3", "grep"), ("g4", "grep"),
            ]),
            tool_result("s1", &skill_output),
            tool_result("r1", &file_content),
            tool_result("r2", &file_content),
            tool_result("r3", &file_content),
            tool_result("r4", &file_content),
            tool_result("r5", &file_content),
            tool_result("r6", &file_content),
            tool_result("r7", &file_content),
            tool_result("r8", &file_content),
            tool_result("r9", &file_content),
            tool_result("r10", &file_content),
            tool_result("r11", &file_content),
            tool_result("g1", &grep_output),
            tool_result("g2", &grep_output),
            tool_result("g3", &grep_output),
            tool_result("g4", &grep_output),
        ];

        // Before iteration 2: run microcompact
        let stats = compact_tool_results(&mut messages, None); // default keep=6

        // 15 compactable (11 read_file + 4 grep), skill is NOT compactable.
        // Count-based: 15 - 6 = 9 to compact.
        // Token-based: 15 * ~240 tokens = ~3600 tokens < 12K budget → no extra.
        assert_eq!(stats.results_compacted, 9);

        // Skill output preserved (not compactable)
        assert_ne!(messages[2]["content"], CLEARED_PLACEHOLDER);
        assert!(messages[2]["content"].as_str().unwrap().contains("Review"));

        // Most recent 6 compactable results preserved
        // (g1, g2, g3, g4 are indices 15-18, r10, r11 are indices 12-13)
        // The last 6 in order: r6..r11? No — compactable order is r1..r11, g1..g4
        // Last 6: r11, g1, g2, g3, g4 + one more = r10
        // Actually: compactable indices are [3..13, 14..17] = r1..r11, g1..g4
        // Last 6: indices for r10, r11, g1, g2, g3, g4

        // Verify oldest are cleared
        assert_eq!(messages[3]["content"], CLEARED_PLACEHOLDER); // r1
        assert_eq!(messages[4]["content"], CLEARED_PLACEHOLDER); // r2

        // Verify newest are kept
        assert_ne!(messages[17]["content"], CLEARED_PLACEHOLDER); // g4
        assert_ne!(messages[16]["content"], CLEARED_PLACEHOLDER); // g3

        // Token savings: 9 results * ~240 tokens each ≈ 2160
        assert!(stats.tokens_saved > 1500, "expected meaningful savings, got {}", stats.tokens_saved);
    }

    #[test]
    fn realistic_large_file_reads_trigger_token_budget() {
        // Scenario: 4 large file reads (each ~4K tokens = 16K bytes).
        // Count < keep=6, but total tokens (16K) > TOKEN_BUDGET (12K).
        let large_file = "x".repeat(16_000); // ~4K tokens each

        let mut messages = vec![
            assistant_with_tools(&[
                ("r1", "read_file"), ("r2", "read_file"),
                ("r3", "read_file"), ("r4", "read_file"),
            ]),
            tool_result("r1", &large_file),
            tool_result("r2", &large_file),
            tool_result("r3", &large_file),
            tool_result("r4", &large_file),
        ];

        let stats = compact_tool_results(&mut messages, None); // keep=6

        // Count-based: 4 < 6 → 0. But token-based: 4*4K = 16K > 12K → must clear some.
        assert!(
            stats.results_compacted >= 1,
            "token budget should trigger on large files, got {} compacted",
            stats.results_compacted
        );

        // Most recent should be preserved
        assert_ne!(messages[4]["content"], CLEARED_PLACEHOLDER); // r4 (newest)

        // Total remaining tokens should be under budget
        let remaining_tokens: usize = messages.iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .filter(|m| m.get("content").and_then(Value::as_str) != Some(CLEARED_PLACEHOLDER))
            .map(|m| estimate_tokens(m.get("content").and_then(Value::as_str).unwrap_or("")))
            .sum();
        assert!(
            remaining_tokens <= TOKEN_BUDGET,
            "remaining tokens {} should be <= budget {}",
            remaining_tokens, TOKEN_BUDGET
        );
    }

    #[test]
    fn mixed_compactable_and_non_compactable_interleaved() {
        // Real pattern: read_file → bash(make test) → read_file → grep
        // bash results must survive even when surrounded by compactable tools.
        let big = "x".repeat(1000);
        let test_output = "test result: ok. 42 passed; 0 failed".repeat(20); // ~720 bytes

        let mut messages = vec![
            assistant_with_tools(&[("r1", "read_file"), ("b1", "bash"), ("r2", "read_file"), ("g1", "grep")]),
            tool_result("r1", &big),
            tool_result("b1", &test_output),
            tool_result("r2", &big),
            tool_result("g1", &big),
        ];

        let stats = compact_tool_results(&mut messages, Some(1));

        // 3 compactable (r1, r2, g1), keep 1 → compact 2 (r1, r2)
        assert_eq!(stats.results_compacted, 2);
        assert_eq!(messages[1]["content"], CLEARED_PLACEHOLDER); // r1 compacted
        assert!(messages[2]["content"].as_str().unwrap().contains("test result")); // bash preserved!
        assert_eq!(messages[3]["content"], CLEARED_PLACEHOLDER); // r2 compacted
        assert_eq!(messages[4]["content"], big); // g1 kept (most recent compactable)
    }

    // ── Goal preservation after compaction ────────────────────────────

    #[test]
    fn goal_context_preserved_after_multi_round_compaction() {
        // Simulate a real multi-round session:
        // Round 1: user asks to fix a bug. LLM reads 8 files, runs tests.
        // Round 2: LLM reads 4 more files, makes a fix.
        // Round 3: LLM runs tests again.
        // After compaction before round 3, verify:
        // - User's original request survives
        // - LLM's analysis/conclusions survive (assistant text)
        // - Test output (bash) survives
        // - File paths in tool_calls survive (LLM knows what to re-read)
        // - Cleared results have placeholder (not deleted)

        let file_content = "fn buggy() { panic!(); }\n".repeat(40); // ~1KB
        let test_fail = "FAILED: test_foo - assertion failed at line 42\nExpected: 5\nGot: 3";
        let test_pass = "test result: ok. 10 passed; 0 failed";

        let mut messages = vec![
            // User goal
            json!({"role": "user", "content": "Fix the bug in src/parser.rs that causes test_foo to fail"}),

            // Round 1: LLM reads files and runs tests
            json!({"role": "assistant", "content": "I'll investigate the test failure. Let me read the relevant files and run the tests.", "tool_calls": [
                {"id": "r1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/parser.rs\"}"}},
                {"id": "r2", "function": {"name": "read_file", "arguments": "{\"path\": \"src/lexer.rs\"}"}},
                {"id": "r3", "function": {"name": "read_file", "arguments": "{\"path\": \"src/ast.rs\"}"}},
                {"id": "r4", "function": {"name": "read_file", "arguments": "{\"path\": \"tests/test_parser.rs\"}"}},
                {"id": "r5", "function": {"name": "read_file", "arguments": "{\"path\": \"src/lib.rs\"}"}},
                {"id": "r6", "function": {"name": "read_file", "arguments": "{\"path\": \"src/error.rs\"}"}},
                {"id": "r7", "function": {"name": "read_file", "arguments": "{\"path\": \"src/token.rs\"}"}},
                {"id": "r8", "function": {"name": "read_file", "arguments": "{\"path\": \"Cargo.toml\"}"}},
                {"id": "b1", "function": {"name": "bash", "arguments": "{\"command\": \"cargo test test_foo\"}"}},
            ]}),
            tool_result("r1", &file_content),
            tool_result("r2", &file_content),
            tool_result("r3", &file_content),
            tool_result("r4", &file_content),
            tool_result("r5", &file_content),
            tool_result("r6", &file_content),
            tool_result("r7", &file_content),
            tool_result("r8", &file_content),
            tool_result("b1", test_fail),

            // Round 1 conclusion
            json!({"role": "assistant", "content": "I found the bug. In src/parser.rs line 42, the parse_expr function returns the wrong precedence value (3 instead of 5). The fix is to change the constant on line 42."}),

            // Round 2: LLM reads more files and applies fix
            json!({"role": "assistant", "content": "Let me apply the fix.", "tool_calls": [
                {"id": "r9", "function": {"name": "read_file", "arguments": "{\"path\": \"src/parser.rs:40-50\"}"}},
                {"id": "r10", "function": {"name": "read_file", "arguments": "{\"path\": \"src/precedence.rs\"}"}},
                {"id": "r11", "function": {"name": "read_file", "arguments": "{\"path\": \"src/constants.rs\"}"}},
                {"id": "r12", "function": {"name": "read_file", "arguments": "{\"path\": \"tests/test_precedence.rs\"}"}},
                {"id": "w1", "function": {"name": "str_replace", "arguments": "{\"path\": \"src/parser.rs\"}"}},
            ]}),
            tool_result("r9", &file_content),
            tool_result("r10", &file_content),
            tool_result("r11", &file_content),
            tool_result("r12", &file_content),
            tool_result("w1", "Applied: replaced '3' with '5' on line 42"),

            // Round 2 conclusion
            json!({"role": "assistant", "content": "Fix applied. Now let me run the tests to verify.", "tool_calls": [
                {"id": "b2", "function": {"name": "bash", "arguments": "{\"command\": \"cargo test\"}"}},
            ]}),
            tool_result("b2", test_pass),
        ];

        // Run compaction (simulating what happens before round 3)
        let stats = compact_tool_results(&mut messages, None);

        // Should compact some old read_file results
        assert!(stats.results_compacted > 0, "should compact old file reads");

        // ── GOAL PRESERVATION CHECKS ──

        // 1. User's original request is intact
        assert!(
            messages[0]["content"].as_str().unwrap().contains("Fix the bug in src/parser.rs"),
            "user's goal must survive compaction"
        );

        // 2. LLM's analysis/conclusions are intact (assistant text)
        let assistant_texts: Vec<&str> = messages.iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        assert!(
            assistant_texts.iter().any(|t| t.contains("parse_expr function returns the wrong precedence")),
            "LLM's bug analysis must survive"
        );
        assert!(
            assistant_texts.iter().any(|t| t.contains("Fix applied")),
            "LLM's fix confirmation must survive"
        );

        // 3. Test outputs (bash) survive — these are critical evidence
        let bash_results: Vec<&str> = messages.iter()
            .filter(|m| {
                m.get("role").and_then(Value::as_str) == Some("tool")
                    && (m.get("tool_call_id").and_then(Value::as_str) == Some("b1")
                        || m.get("tool_call_id").and_then(Value::as_str) == Some("b2"))
            })
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        assert!(
            bash_results.iter().any(|t| t.contains("FAILED")),
            "original test failure output must survive"
        );
        assert!(
            bash_results.iter().any(|t| t.contains("ok. 10 passed")),
            "test pass output must survive"
        );

        // 4. str_replace result survives (mutation record)
        let w1_content = messages.iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("w1"))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap();
        assert!(
            w1_content.contains("Applied"),
            "write/edit result must survive: got '{}'", w1_content
        );

        // 5. File paths in tool_calls survive (LLM can re-read if needed)
        let all_tool_calls: Vec<&str> = messages.iter()
            .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
            .flat_map(|calls| calls.iter())
            .filter_map(|tc| tc.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str))
            .collect();
        assert!(
            all_tool_calls.iter().any(|a| a.contains("src/parser.rs")),
            "file paths in tool_calls must survive for re-reading"
        );

        // 6. Cleared results have placeholder, not deleted
        let cleared: Vec<&Value> = messages.iter()
            .filter(|m| {
                m.get("content").and_then(Value::as_str) == Some(CLEARED_PLACEHOLDER)
            })
            .collect();
        assert!(
            !cleared.is_empty(),
            "some results should be cleared"
        );
        for msg in &cleared {
            assert!(
                msg.get("tool_call_id").is_some(),
                "cleared results must retain tool_call_id for context"
            );
        }

        // 7. Total message count unchanged (no messages deleted)
        assert_eq!(messages.len(), 20, "no messages should be deleted, only content replaced");
    }
}
