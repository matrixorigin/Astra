//! Write-time gate for persistent memory ingestion.
//!
//! Answers one question: **should this message be stored into persistent
//! memory (Memoria working-memory, etc.)?** Returning `false` keeps the
//! payload out of the index entirely — no ambient retrieval can bring
//! it back.
//!
//! Motivation — systemic, not whack-a-mole
//! =======================================
//! Read-time filters (`is_memory_worthy`, `is_digest_worthy`) keep
//! accumulating exclusion prefixes, but the index itself still grows
//! with every low-signal message the runtime ever saw. Each new session
//! re-reads the pollution from prior sessions because embeddings find
//! content overlap on commands like "继续啊", "修复", "hi" — all of
//! which were legitimate user inputs, just without any durable signal.
//!
//! Claude Code addresses this by making memory a **curated disk
//! directory**: each memory is a file with frontmatter, Claude picks a
//! `type` (user / feedback / project / reference) at write-time, and a
//! system-prompt section spells out **what NOT to save** (code patterns,
//! git history, ephemeral task details). We can't port the whole
//! architecture in one go, but we can adopt the first principle:
//! **reject obvious non-memories at the point of storage** so later
//! retrieval doesn't need to filter them.
//!
//! Scope — deliberately narrow
//! ===========================
//! This module makes the **L1 fix**: reject ephemeral / content-free
//! messages. L2 (require `[@ns/type]` prefix on stored payloads) and
//! L3 (replace Memoria with a directory + frontmatter) are follow-up
//! projects.
//!
//! What we reject (observed live in session `c6e18730`):
//!   - bare user acknowledgments / imperatives under a length threshold
//!     ("继续", "修复啊！", "hi", "好", "ok")
//!   - runtime scaffolding (delegated to
//!     [`crate::runtime_scaffolding::is_runtime_scaffolding_message`])
//!
//! What we keep:
//!   - assistant messages — the model's own output is expensive to
//!     regenerate and usually carries real analysis/decisions
//!   - longer user messages (above the threshold) — those carry facts,
//!     preferences, or explicit decisions worth remembering
//!   - tool messages are the caller's problem (working memory skips
//!     tool role entirely)

use serde_json::Value;

use crate::runtime_scaffolding::is_runtime_scaffolding_message;

/// User-message length threshold (in Unicode scalar values, not bytes)
/// below which the message is treated as an ephemeral ack / imperative
/// and skipped from persistent storage.
///
/// 20 scalars covers:
///  - single-word English acks: "ok", "yes", "good", "done"
///  - single-word Chinese acks: "好", "对", "继续啊" (3 chars)
///  - short imperatives: "继续修复", "修复啊！", "review pr"
///
/// Longer messages — "Use RS256 for JWT", "I'm a senior Rust engineer",
/// "Focus on the auth middleware not the schema migration" — carry
/// concrete facts / preferences / constraints and pass through.
///
/// This isn't a hard line; a 19-char English sentence like "the fix is
/// in auth.rs" would get rejected. That's OK: the code fix is in the
/// diff, so the memory would repeat what git already records. Claude
/// Code's "what NOT to save" list deliberately calls out this class.
const USER_MSG_MIN_CHARS: usize = 20;

/// True when the message carries durable signal that's worth storing
/// into persistent memory. False for runtime scaffolding and short
/// ephemeral user inputs.
///
/// Single source of truth: callers (compaction, memory tool, session-end
/// knowledge writer) should route writes through this predicate rather
/// than duplicating the policy.
#[must_use]
pub fn should_store_in_memory(message: &Value) -> bool {
    if is_runtime_scaffolding_message(message) {
        return false;
    }

    let role = message.get("role").and_then(Value::as_str).unwrap_or("");

    // Assistant-with-tool_calls: content is typically `null` but the
    // tool-call list itself carries durable signal ("what tools did
    // the model invoke"). Keep these unconditionally — the caller is
    // responsible for rendering the tool_calls into its stored form
    // (e.g. `Assistant: [tools: bash]`). Scaffolding filter above has
    // already dropped runtime-injected assistant messages by content.
    if role == "assistant"
        && message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    {
        return true;
    }

    // Skip messages with no parseable string content (null, arrays
    // other than tool_calls, structured content we can't render as
    // text). Working-memory writers treat these cases specially;
    // unclear content isn't durable signal.
    let content = match message.get("content").and_then(Value::as_str) {
        Some(s) => s.trim(),
        None => return false,
    };

    if content.is_empty() {
        return false;
    }

    match role {
        // Assistant text output is expensive to regenerate; always keep.
        "assistant" => true,
        // User messages pass the length gate.
        "user" => content.chars().count() >= USER_MSG_MIN_CHARS,
        // Tool role, system role, unknown role: don't write. Tools are
        // handled separately by compaction (which skips `role: "tool"`
        // entirely); system messages are runtime-injected by contract.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(content: &str) -> Value {
        json!({"role": "user", "content": content})
    }

    fn assistant(content: &str) -> Value {
        json!({"role": "assistant", "content": content})
    }

    // ── Reject: runtime scaffolding ───────────────────────────────────
    // Delegates to the existing scaffolding detector. These tests pin
    // the wire-up so a regression in either module is caught here.

    #[test]
    fn rejects_scaffolding_nudge() {
        assert!(!should_store_in_memory(&user(
            "## ⚠ Sequential Tool Calls Detected\n..."
        )));
    }

    #[test]
    fn rejects_attention_manifest() {
        assert!(!should_store_in_memory(&user(
            "[attention:v1]\ngoal: fix bug"
        )));
    }

    #[test]
    fn rejects_system_messages() {
        assert!(!should_store_in_memory(
            &json!({"role": "system", "content": "any content"})
        ));
    }

    // ── Reject: short user acks / imperatives ──────────────────────────
    // Every phrase here is real text from session `c6e18730` that
    // Memoria ingested and surfaced as a "memory" on a later turn.

    #[test]
    fn rejects_short_chinese_ack() {
        // "继续啊" = 3 CJK chars — well below threshold.
        assert!(!should_store_in_memory(&user("继续啊")));
    }

    #[test]
    fn rejects_short_chinese_imperative() {
        // "修复啊！" = 4 chars + punctuation. No concrete facts.
        assert!(!should_store_in_memory(&user("修复啊！")));
    }

    #[test]
    fn rejects_continue_phrases() {
        for msg in ["继续", "好", "对", "ok", "yes", "done", "继续修复"] {
            assert!(
                !should_store_in_memory(&user(msg)),
                "should reject ephemeral ack {msg:?}"
            );
        }
    }

    #[test]
    fn rejects_hi() {
        assert!(!should_store_in_memory(&user("hi")));
        assert!(!should_store_in_memory(&user("hello")));
    }

    // ── Reject: pathological shapes ────────────────────────────────────

    #[test]
    fn rejects_empty_content() {
        assert!(!should_store_in_memory(&user("")));
        assert!(!should_store_in_memory(&user("   \n\t  ")));
    }

    #[test]
    fn keeps_assistant_with_tool_calls_even_null_content() {
        // Assistant-with-tool_calls is the normal shape for a
        // tool-invoking turn (content null, tool_calls populated).
        // The tool-call list IS the durable signal — which tools the
        // model chose to invoke in what context. Working-memory
        // writers render it as `Assistant: [tools: bash, read_file]`;
        // keeping these lets future turns see the tool-use pattern.
        let m = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"function": {"name": "bash"}}]
        });
        assert!(should_store_in_memory(&m));
    }

    #[test]
    fn rejects_assistant_null_without_tool_calls() {
        // Null content + no tool_calls is pathological (shouldn't
        // happen normally); nothing to store.
        let m = json!({"role": "assistant", "content": null});
        assert!(!should_store_in_memory(&m));
    }

    #[test]
    fn rejects_assistant_null_with_empty_tool_calls() {
        let m = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": []
        });
        assert!(!should_store_in_memory(&m));
    }

    #[test]
    fn rejects_unknown_role() {
        assert!(!should_store_in_memory(
            &json!({"role": "tool", "content": "file contents"})
        ));
        assert!(!should_store_in_memory(
            &json!({"role": "gibberish", "content": "whatever"})
        ));
    }

    // ── Keep: real user intent ─────────────────────────────────────────

    #[test]
    fn keeps_user_preference() {
        // ≥20 chars of concrete preference — the class of content that
        // justifies persistent memory.
        assert!(should_store_in_memory(&user(
            "I prefer Rust for CLI tools; default to cargo for builds."
        )));
    }

    #[test]
    fn keeps_user_constraint() {
        assert!(should_store_in_memory(&user(
            "don't mock the database in these tests — we got burned last quarter"
        )));
    }

    #[test]
    fn keeps_user_multistep_request() {
        // Real task description, long enough to carry structure.
        assert!(should_store_in_memory(&user(
            "Add OAuth support to the API with JWT tokens and refresh rotation."
        )));
    }

    // ── Keep: assistant output always ──────────────────────────────────

    #[test]
    fn keeps_assistant_short_output() {
        // Even short assistant output passes — regenerating model text
        // is expensive, and conversational context often needs it.
        assert!(should_store_in_memory(&assistant("Done.")));
    }

    #[test]
    fn keeps_assistant_long_output() {
        let msg = assistant(
            "Fixed the auth bug. Root cause: session token comparison used == \
             instead of constant-time compare, enabling timing attacks. \
             Changed src/auth.rs to use subtle::ConstantTimeEq.",
        );
        assert!(should_store_in_memory(&msg));
    }

    // ── Boundary test: length threshold ────────────────────────────────

    #[test]
    fn threshold_is_20_unicode_chars() {
        // 19 scalars → reject
        let short = "a".repeat(19);
        assert!(!should_store_in_memory(&user(&short)));
        // 20 scalars → keep
        let exact = "a".repeat(20);
        assert!(should_store_in_memory(&user(&exact)));
    }

    #[test]
    fn threshold_counts_scalars_not_bytes() {
        // 10 CJK chars = 30 bytes — well above the byte-count threshold
        // if we were using bytes, but only 10 scalars. Should REJECT.
        let cjk = "测试".repeat(5); // 10 CJK scalars
        assert!(
            !should_store_in_memory(&user(&cjk)),
            "CJK strings must be measured in scalars, not bytes"
        );
    }

    // ── Composition: scaffolding check runs first ──────────────────────

    #[test]
    fn scaffolding_wins_over_length_gate() {
        // A long scaffolding message is still scaffolding. Don't let
        // length pass something through that the scaffolding filter
        // would reject.
        let long_scaffold = user(&format!(
            "## ⚠ Sequential Tool Calls Detected\n{}",
            "x".repeat(500)
        ));
        assert!(!should_store_in_memory(&long_scaffold));
    }
}
