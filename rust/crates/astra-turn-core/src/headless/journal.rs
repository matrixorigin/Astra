//! `ToolCallRecord` rows for headless early-exit paths.

use astra_services::session_journal::{NOOP_OR_CACHED_RESULT_CLASS, ToolCallRecord};

#[must_use]
pub fn journal_record_duplicate_within_turn(
    name: String,
    args_preview: Option<String>,
) -> ToolCallRecord {
    ToolCallRecord {
        name,
        ok: true,
        ms: 0,
        error: Some("duplicate_within_turn".to_string()),
        input_bytes: None,
        output_bytes: None,
        args_preview,
        result_preview: None,
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        result_class: Some(NOOP_OR_CACHED_RESULT_CLASS.to_string()),
        ..Default::default()
    }
}

/// Maximum number of bytes from the cached body to embed in the
/// journal's `result_preview` when a snippet is available. Keeps
/// forensics useful (you can see *what* was reused) without
/// blowing up the journal with duplicate content — the full body
/// is already in the earlier turn the cache hit refers to.
/// Byte ceiling for the cached-body snippet appended to a cross-turn cache
/// hit's `result_preview`. We truncate on a UTF-8 char boundary at or below
/// this many bytes so the snippet stays bounded regardless of how wide the
/// source encoding is, and never panics on multi-byte input.
const CACHE_HIT_PREVIEW_SNIPPET_BYTES: usize = 400;

/// Return the longest prefix of `s` whose byte length is `≤ max_bytes` that
/// still ends on a UTF-8 char boundary, together with a flag indicating
/// whether any content was dropped.
pub fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    // Walk char boundaries and keep the largest one that fits.
    let mut end = 0;
    for (idx, ch) in s.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    (&s[..end], true)
}

/// Record a cross-turn cache hit.
///
/// Populates `result_preview` with a `[cached_cross_turn: ...]`
/// tagged string so downstream analysis (digest, LLM self-
/// diagnosis) can distinguish "cache reused N bytes" from "tool
/// returned empty body".
///
/// `cached_body` is optional because some short-circuit paths
/// (e.g. pre-suppressed repeated cache hits) don't have the full
/// body handy; when absent, the preview still carries the byte
/// count and tag so it's self-identifying.
#[must_use]
pub fn journal_record_cross_turn_cache_hit(
    name: String,
    output_len: u32,
    args_preview: Option<String>,
    cached_body: Option<&str>,
) -> ToolCallRecord {
    let preview = format_cross_turn_cache_hit_preview(output_len, cached_body);
    ToolCallRecord {
        name,
        ok: true,
        ms: 0,
        error: Some("cached_cross_turn".to_string()),
        input_bytes: None,
        output_bytes: Some(output_len),
        args_preview,
        result_preview: Some(preview),
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        result_class: Some(NOOP_OR_CACHED_RESULT_CLASS.to_string()),
        ..Default::default()
    }
}

fn format_cross_turn_cache_hit_preview(output_len: u32, cached_body: Option<&str>) -> String {
    let tag = format!("[cached_cross_turn: reused {output_len} bytes from earlier turn]");
    match cached_body {
        Some(body) if !body.is_empty() => {
            // Truncate at a char boundary to avoid splitting UTF-8
            // codepoints. The tag stays first so scanners keying on
            // the prefix don't have to strip a snippet header.
            let (snippet, truncated) =
                truncate_on_char_boundary(body, CACHE_HIT_PREVIEW_SNIPPET_BYTES);
            let ellipsis = if truncated { "…" } else { "" };
            format!("{tag}\n{snippet}{ellipsis}")
        }
        _ => tag,
    }
}

#[must_use]
pub fn journal_record_unknown_tool(name: String, tool_elapsed_ms: u64) -> ToolCallRecord {
    ToolCallRecord {
        name: name.clone(),
        ok: false,
        ms: tool_elapsed_ms,
        error: Some(format!("unknown_tool: {name}")),
        input_bytes: None,
        output_bytes: None,
        args_preview: None,
        result_preview: None,
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
    }
}

#[must_use]
pub fn journal_record_tool_not_admitted(
    name: String,
    args_preview: Option<String>,
    reason: &str,
    tool_elapsed_ms: u64,
) -> ToolCallRecord {
    let preview: String = format!("Deferred: {reason}").chars().take(500).collect();
    ToolCallRecord {
        name,
        ok: false,
        ms: tool_elapsed_ms,
        error: Some("tool_not_admitted".to_string()),
        input_bytes: None,
        output_bytes: None,
        args_preview,
        result_preview: Some(preview),
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        result_class: None,
        ..Default::default()
    }
}

#[must_use]
pub fn journal_record_blocked_tool(
    name: String,
    reason: String,
    args_preview: Option<String>,
    tool_elapsed_ms: u64,
) -> ToolCallRecord {
    ToolCallRecord {
        name,
        ok: false,
        ms: tool_elapsed_ms,
        error: Some(format!("blocked_tool: {reason}")),
        input_bytes: None,
        output_bytes: None,
        args_preview,
        result_preview: None,
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
    }
}

#[must_use]
pub fn journal_record_suppressed_tool_retry(
    name: String,
    reason_code: &str,
    reason: String,
    args_preview: Option<String>,
    tool_elapsed_ms: u64,
) -> ToolCallRecord {
    let preview: String = format!("Deferred: {reason}").chars().take(500).collect();
    ToolCallRecord {
        name,
        ok: true,
        ms: tool_elapsed_ms,
        error: Some(reason_code.to_string()),
        input_bytes: None,
        output_bytes: None,
        args_preview,
        result_preview: Some(preview),
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        result_class: Some(NOOP_OR_CACHED_RESULT_CLASS.to_string()),
        ..Default::default()
    }
}

#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn journal_record_executed_tool_call(
    name: String,
    is_err: bool,
    tool_elapsed_ms: u64,
    args_size: u32,
    result_str: &str,
    args_preview: Option<String>,
    file_path: Option<String>,
    args_full: Option<String>,
) -> ToolCallRecord {
    // Truncate to 500 chars for cloud audit (up from 200, multi-line)
    let preview: String = result_str.chars().take(500).collect();
    let result_preview = if preview.is_empty() {
        None
    } else {
        Some(if result_str.chars().count() > 500 {
            format!("{preview}…")
        } else {
            preview
        })
    };

    // Store full result up to 50 KB. Larger outputs (bash, read_file) are
    // already persisted to tool-results/<call_id>.txt by tool_result_storage.
    const MAX_RESULT_FULL_BYTES: usize = 50_000;
    let result_full = if result_str.is_empty() || result_str.len() > MAX_RESULT_FULL_BYTES {
        None
    } else {
        Some(result_str.to_string())
    };

    ToolCallRecord {
        name,
        ok: !is_err,
        ms: tool_elapsed_ms,
        error: if is_err {
            // Keep up to 500 chars of error (multi-line) for better diagnostics
            Some(result_str.chars().take(500).collect())
        } else {
            None
        },
        input_bytes: Some(args_size),
        output_bytes: Some(result_str.len() as u32),
        args_preview,
        result_preview,
        file_path,
        surgically_removed: None,
        original_tool_name: None,
        args_full,
        result_full,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_record_fields() {
        let r = journal_record_duplicate_within_turn("bash".into(), Some("x".into()));
        assert!(r.ok);
        assert_eq!(r.error.as_deref(), Some("duplicate_within_turn"));
        assert_eq!(r.result_class.as_deref(), Some(NOOP_OR_CACHED_RESULT_CLASS));
        assert!(r.is_structured_noop_or_cached_result());
    }

    #[test]
    fn cache_hit_record_has_output_bytes() {
        let r = journal_record_cross_turn_cache_hit("read_file".into(), 12, None, None);
        assert_eq!(r.output_bytes, Some(12));
        assert_eq!(r.result_class.as_deref(), Some(NOOP_OR_CACHED_RESULT_CLASS));
        assert!(r.is_structured_noop_or_cached_result());
    }

    #[test]
    fn cache_hit_record_old_api_remains_source_compatible() {
        let r = journal_record_cross_turn_cache_hit("read_file".into(), 12, None, None);
        assert_eq!(r.output_bytes, Some(12));
        assert!(
            r.result_preview
                .as_deref()
                .is_some_and(|preview| preview.contains("12 bytes")),
            "legacy API should still emit a cache-hit preview: {r:?}"
        );
    }

    #[test]
    fn cache_hit_record_preview_populated_when_source_provided() {
        // Regression (session 6d6c1041): cache-hit records used to
        // leave `result_preview: None`, which makes the journal
        // look like the tool returned an empty body.  Downstream
        // analysis (`astra journal digest`, LLM self-diagnosis) was
        // mis-led into reporting "empty output" and, in one case,
        // hallucinating a `{}`-return bug.  The fix is to populate
        // `result_preview` with a synthetic explanatory string that
        // makes the cache-hit nature explicit.
        let r = journal_record_cross_turn_cache_hit(
            "read_file".into(),
            2000,
            Some("src/lib.rs".into()),
            Some("fn main() {\n    println!(\"hi\");\n}\n// ... many more lines ..."),
        );
        let preview = r.result_preview.expect("cache-hit must carry a preview");
        assert!(
            preview.starts_with("[cached_cross_turn:"),
            "preview must be tagged so analysis tools can classify it: {preview:?}"
        );
        assert!(
            preview.contains("2000 bytes"),
            "preview should state the reused byte count: {preview:?}"
        );
        assert!(
            preview.contains("fn main"),
            "preview should include a snippet of the cached content for forensics: {preview:?}"
        );
    }

    #[test]
    fn cache_hit_preview_keeps_short_multibyte_body_intact() {
        // 100 Han characters = 300 bytes — safely under the 400-byte
        // snippet ceiling. Every glyph must survive the preview
        // intact (no panic, no truncation, no replacement char).
        let body: String = "中".repeat(100);
        assert_eq!(body.len(), 300, "setup: 100 Han chars must be 300 bytes");
        let r = journal_record_cross_turn_cache_hit(
            "read_file".into(),
            body.len() as u32,
            None,
            Some(&body),
        );
        let preview = r.result_preview.expect("must carry preview");
        // All 100 input glyphs are preserved in the preview body.
        assert_eq!(preview.matches('中').count(), 100);
        // No ellipsis because body fit under the ceiling.
        assert!(
            !preview.ends_with('…'),
            "no ellipsis expected when body fits: {preview:?}"
        );
        assert!(
            !preview.contains('\u{FFFD}'),
            "no U+FFFD replacement char allowed: {preview:?}"
        );
    }

    #[test]
    fn cache_hit_preview_truncates_cjk_on_char_boundary_not_mid_codepoint() {
        // 200 Han characters = 600 bytes — exceeds the 400-byte
        // snippet ceiling. Must truncate on a char boundary; the
        // naive `&s[..400]` would panic (400 is mid-codepoint for
        // 3-byte UTF-8 runs: 400 % 3 != 0).
        let body: String = "中".repeat(200);
        assert_eq!(body.len(), 600, "setup: 200 Han chars must be 600 bytes");

        let r = journal_record_cross_turn_cache_hit(
            "read_file".into(),
            body.len() as u32,
            None,
            Some(&body),
        );
        let preview = r.result_preview.expect("must carry preview");
        assert!(
            preview.ends_with('…'),
            "truncation should append an ellipsis: {preview:?}"
        );

        // Extract the snippet between the tag line and the trailing
        // ellipsis.  The prefix is fixed per
        // format_cross_turn_cache_hit_preview.
        let tag_line = "[cached_cross_turn: reused 600 bytes from earlier turn]\n";
        let snippet = preview
            .strip_prefix(tag_line)
            .expect("preview must start with tag line")
            .trim_end_matches('…');

        // Every surviving char is still `中` — none was sliced mid-codepoint.
        assert!(
            snippet.chars().all(|c| c == '中'),
            "all chars should be intact 中, got {snippet:?}"
        );
        assert!(
            !snippet.contains('\u{FFFD}'),
            "no U+FFFD replacement char allowed"
        );

        // Snippet bytes must be ≤ 400 (strictly under because 400 is
        // not on a Han char boundary — largest fit is 399 bytes = 133 chars).
        assert!(
            snippet.len() <= 400,
            "snippet length must respect the byte ceiling: got {}",
            snippet.len()
        );
        assert_eq!(
            snippet.chars().count(),
            133,
            "largest intact Han-char run fitting 400 bytes is 133 × 3 = 399 bytes"
        );
    }

    #[test]
    fn truncate_on_char_boundary_handles_exact_fit() {
        // When the body length exactly equals the ceiling we must
        // NOT flag it as truncated (no ellipsis) — the body fits.
        let body: String = "中".repeat(100); // 300 bytes
        let (out, truncated) = truncate_on_char_boundary(&body, 300);
        assert_eq!(out, body.as_str());
        assert!(!truncated);
    }

    #[test]
    fn truncate_on_char_boundary_rejects_mixed_ascii_and_emoji() {
        // ASCII + 4-byte emoji: make sure the helper prefers the
        // largest char boundary ≤ the ceiling and never produces an
        // invalid slice.
        let body = "abc🎉🎉🎉"; // 3 ASCII + 3 × 4-byte emoji = 15 bytes
        // Ceiling = 10: fits "abc" (3) + 1 emoji (4) = 7 bytes.
        // Adding the next emoji would take us to 11 bytes > 10.
        let (out, truncated) = truncate_on_char_boundary(body, 10);
        assert!(truncated);
        assert_eq!(out, "abc🎉");
        assert_eq!(out.len(), 7);
    }

    #[test]
    fn cache_hit_record_preview_omits_snippet_when_source_absent() {
        // When the caller can't hand us the cached body (e.g. a
        // pre-suppressed "repeated cache hit" short-circuit), we
        // still emit a non-empty preview so downstream tooling
        // isn't misled.
        let r = journal_record_cross_turn_cache_hit(
            "read_file".into(),
            1024,
            Some("src/lib.rs".into()),
            None,
        );
        let preview = r
            .result_preview
            .expect("cache-hit with no snippet must still carry a preview");
        assert!(preview.starts_with("[cached_cross_turn:"));
        assert!(preview.contains("1024 bytes"));
    }

    #[test]
    fn unknown_tool_error_tag() {
        let r = journal_record_unknown_tool("nope".into(), 7);
        assert!(!r.ok);
        assert_eq!(r.ms, 7);
        assert_eq!(r.error.as_deref(), Some("unknown_tool: nope"));
        assert_eq!(r.result_class, None);
        assert!(!r.is_structured_noop_or_cached_result());
    }

    #[test]
    fn tool_not_admitted_record_is_protocol_failure_not_synthetic_success() {
        let r = journal_record_tool_not_admitted(
            "agent_fanout".into(),
            Some("{}".into()),
            "Tool 'agent_fanout' must be activated first",
            3,
        );
        assert!(!r.ok);
        assert_eq!(r.ms, 3);
        assert_eq!(r.error.as_deref(), Some("tool_not_admitted"));
        assert_eq!(r.args_preview.as_deref(), Some("{}"));
        assert!(
            r.result_preview
                .as_deref()
                .is_some_and(|preview| preview.starts_with("Deferred:"))
        );
        assert!(!r.is_synthetic_placeholder());
        assert_eq!(r.result_class.as_deref(), None);
        assert!(!r.is_structured_noop_or_cached_result());
    }

    #[test]
    fn blocked_tool_error_tag() {
        let r = journal_record_blocked_tool(
            "bash".into(),
            "denied by policy".into(),
            Some(r#"{"command":"echo hi"}"#.into()),
            9,
        );
        assert!(!r.ok);
        assert_eq!(r.ms, 9);
        assert_eq!(r.error.as_deref(), Some("blocked_tool: denied by policy"));
        assert_eq!(r.args_preview.as_deref(), Some(r#"{"command":"echo hi"}"#));
    }

    #[test]
    fn suppressed_retry_record_is_synthetic_success() {
        let r = journal_record_suppressed_tool_retry(
            "agent".into(),
            "nonprogress_retry_deferred",
            "retry later".into(),
            Some(r#"{"action":"get_result"}"#.into()),
            0,
        );
        assert!(r.ok);
        assert_eq!(r.error.as_deref(), Some("nonprogress_retry_deferred"));
        assert!(r.is_synthetic_placeholder());
        assert_eq!(r.result_class.as_deref(), Some(NOOP_OR_CACHED_RESULT_CLASS));
        assert!(r.is_structured_noop_or_cached_result());
        assert!(
            !r.was_blocked_by_policy(),
            "retry deferral must not look like a hard policy block"
        );
    }

    #[test]
    fn executed_record_truncates_error_to_500_chars() {
        let r = journal_record_executed_tool_call(
            "bash".into(),
            true,
            10,
            2,
            "first line\nrest",
            None,
            None,
            None,
        );
        // Now keeps multi-line errors (up to 500 chars)
        assert_eq!(r.error.as_deref(), Some("first line\nrest"));
        assert_eq!(r.output_bytes, Some(15));
        // result_preview also populated for errors
        assert_eq!(r.result_preview.as_deref(), Some("first line\nrest"));
    }

    #[test]
    fn executed_record_result_preview_truncates_long_output() {
        let long_output = "x".repeat(600);
        let r = journal_record_executed_tool_call(
            "grep".into(),
            false,
            5,
            10,
            &long_output,
            None,
            None,
            None,
        );
        assert!(r.ok);
        assert!(r.error.is_none());
        let preview = r.result_preview.unwrap();
        assert_eq!(preview.chars().count(), 501); // 500 + "…"
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn executed_record_error_truncates_at_500_chars() {
        let long_error = "E".repeat(600);
        let r = journal_record_executed_tool_call(
            "bash".into(),
            true,
            5,
            10,
            &long_error,
            None,
            None,
            None,
        );
        assert_eq!(r.error.unwrap().len(), 500);
    }

    #[test]
    fn executed_record_stores_full_args_and_result() {
        let full_args = r#"{"path":"src/main.rs","offset":100,"limit":50}"#;
        let full_result = "x".repeat(1000);
        let r = journal_record_executed_tool_call(
            "read_file".into(),
            false,
            12,
            full_args.len() as u32,
            &full_result,
            Some("src/main.rs".into()),
            Some("src/main.rs".into()),
            Some(full_args.to_string()),
        );
        assert_eq!(
            r.args_full.as_deref(),
            Some(full_args),
            "args_full must store untruncated args"
        );
        assert_eq!(
            r.result_full.as_ref().map(|s| s.len()),
            Some(1000),
            "result_full must store untruncated result"
        );
        // previews are still truncated
        assert!(r.result_preview.unwrap().chars().count() <= 501);
    }

    #[test]
    fn executed_record_full_fields_none_when_not_provided() {
        let r =
            journal_record_executed_tool_call("bash".into(), false, 5, 10, "ok", None, None, None);
        assert!(r.args_full.is_none());
        assert_eq!(r.result_full.as_deref(), Some("ok"));
    }

    #[test]
    fn executed_record_result_full_capped_at_50kb() {
        let large = "x".repeat(51_000);
        let r = journal_record_executed_tool_call(
            "bash".into(),
            false,
            5,
            10,
            &large,
            None,
            None,
            None,
        );
        assert!(
            r.result_full.is_none(),
            "result_full must be None for outputs > 50KB"
        );
        assert!(r.result_preview.is_some(), "result_preview still populated");
    }
}
