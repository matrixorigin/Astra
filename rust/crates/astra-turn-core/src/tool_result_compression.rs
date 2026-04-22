//! Semantic tool-result compression for context budgets (gap #2).
//!
//! Complements [`crate::tool_result_sanitize::MAX_TOOL_RESULT_CHARS`] with
//! **type-aware** shrinking that preserves structure and meaning when the
//! raw char-truncator would produce hash salad.
//!
//! ## Dispatch
//!
//! Given a `(tool_name, content, budget_chars)` triple,
//! [`compress_result_for_context`] tries in order:
//!
//! 1. **No-op** — content already within budget.
//! 2. **JSON array** — first N + last M items preserved; middle elided
//!    with a summary count.
//! 3. **JSON object** — first N keys preserved with compact values;
//!    remaining keys elided as a count.
//! 4. **Line listing** — any newline-rich payload (grep output,
//!    list_dir, git log): keep head + tail lines, summarize the middle.
//! 5. **Error payload** — detected by keywords (`error`, `panic`,
//!    `traceback`); keep the first / last 10 lines each (usually the
//!    top message and deepest frames) and elide the middle.
//! 6. **Fallback** — head + tail raw slice with a notice. Same shape as
//!    the existing char-truncation used by `sanitize`.
//!
//! The compressor is **pure** and has no I/O. It is *advisory*: callers
//! are free to keep using the existing sanitizer when semantic awareness
//! isn't worth the analysis cost.

use serde_json::Value;

/// Notice inserted into compressed output so callers (and the model) can
/// tell the content was shortened rather than simply truncated at a hard
/// limit.
pub const COMPRESSION_MARKER: &str = "\n[… compressed: ";

/// Default budget when callers don't supply an explicit one. Kept below
/// `MAX_TOOL_RESULT_CHARS` so compression activates *before* hard
/// truncation and preserves structure.
pub const DEFAULT_COMPRESSION_BUDGET_CHARS: usize = 8_000;

/// Head / tail line counts for line-listing compression.
const LISTING_HEAD_LINES: usize = 20;
const LISTING_TAIL_LINES: usize = 10;

/// Head / tail item counts for JSON-array compression.
const JSON_ARRAY_HEAD: usize = 10;
const JSON_ARRAY_TAIL: usize = 5;

/// Key budget for JSON-object compression.
const JSON_OBJECT_KEY_LIMIT: usize = 12;

/// Error-payload head/tail line counts.
const ERROR_HEAD_LINES: usize = 10;
const ERROR_TAIL_LINES: usize = 10;

/// Top-level entry point. If `content` already fits the budget, it is
/// returned unchanged. Otherwise a type-aware compressor is selected.
pub fn compress_result_for_context(
    tool_name: &str,
    content: &str,
    budget_chars: usize,
) -> String {
    if content.len() <= budget_chars {
        return content.to_string();
    }
    if let Some(compressed) = try_compress_json(content, budget_chars) {
        return compressed;
    }
    if looks_like_error(tool_name, content) {
        return compress_error(content);
    }
    if is_line_listing(content) {
        return compress_listing(content);
    }
    fallback_head_tail(content, budget_chars)
}

/// Convenience wrapper using the default budget.
pub fn compress_with_default_budget(tool_name: &str, content: &str) -> String {
    compress_result_for_context(tool_name, content, DEFAULT_COMPRESSION_BUDGET_CHARS)
}

// ── JSON compression ──────────────────────────────────────────────────────

fn try_compress_json(content: &str, budget_chars: usize) -> Option<String> {
    let v: Value = serde_json::from_str(content.trim()).ok()?;
    match v {
        Value::Array(arr) => Some(compress_json_array(&arr, budget_chars)),
        Value::Object(_) => Some(compress_json_object(&v, budget_chars)),
        _ => None,
    }
}

fn compress_json_array(arr: &[Value], _budget: usize) -> String {
    let total = arr.len();
    if total <= JSON_ARRAY_HEAD + JSON_ARRAY_TAIL {
        return serde_json::to_string(arr).unwrap_or_default();
    }
    let head: Vec<&Value> = arr.iter().take(JSON_ARRAY_HEAD).collect();
    let tail: Vec<&Value> = arr
        .iter()
        .skip(total.saturating_sub(JSON_ARRAY_TAIL))
        .collect();
    let elided = total - JSON_ARRAY_HEAD - JSON_ARRAY_TAIL;
    let mut out = String::from("[");
    append_items(&mut out, &head);
    out.push_str(&format!(
        ",\"{COMPRESSION_MARKER}{elided} array items elided …]\"",
    ));
    if !tail.is_empty() {
        out.push(',');
        append_items(&mut out, &tail);
    }
    out.push(']');
    out
}

fn append_items(out: &mut String, items: &[&Value]) {
    let parts: Vec<String> = items
        .iter()
        .filter_map(|v| serde_json::to_string(*v).ok())
        .collect();
    out.push_str(&parts.join(","));
}

fn compress_json_object(v: &Value, _budget: usize) -> String {
    let Value::Object(obj) = v else {
        return v.to_string();
    };
    if obj.len() <= JSON_OBJECT_KEY_LIMIT {
        return v.to_string();
    }
    let mut kept = serde_json::Map::new();
    for (k, val) in obj.iter().take(JSON_OBJECT_KEY_LIMIT) {
        kept.insert(k.clone(), val.clone());
    }
    let elided = obj.len() - JSON_OBJECT_KEY_LIMIT;
    kept.insert(
        "__compressed__".into(),
        Value::String(format!("{elided} more keys elided")),
    );
    Value::Object(kept).to_string()
}

// ── Listing compression ───────────────────────────────────────────────────

fn is_line_listing(content: &str) -> bool {
    let line_count = content.lines().count();
    line_count >= (LISTING_HEAD_LINES + LISTING_TAIL_LINES + 1)
}

fn compress_listing(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let elided = total.saturating_sub(LISTING_HEAD_LINES + LISTING_TAIL_LINES);
    if elided == 0 {
        return content.to_string();
    }
    let mut out = String::new();
    for l in lines.iter().take(LISTING_HEAD_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!(
        "{COMPRESSION_MARKER}{elided} middle lines elided …]\n"
    ));
    for l in lines.iter().skip(total - LISTING_TAIL_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    out
}

// ── Error compression ─────────────────────────────────────────────────────

fn looks_like_error(tool_name: &str, content: &str) -> bool {
    if tool_name.contains("error") {
        return true;
    }
    let lower = content.to_ascii_lowercase();
    lower.contains("traceback")
        || lower.contains("panic:")
        || lower.contains("error:")
        || lower.contains("exception:")
}

fn compress_error(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total <= ERROR_HEAD_LINES + ERROR_TAIL_LINES {
        return content.to_string();
    }
    let elided = total - ERROR_HEAD_LINES - ERROR_TAIL_LINES;
    let mut out = String::new();
    for l in lines.iter().take(ERROR_HEAD_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!(
        "{COMPRESSION_MARKER}{elided} middle frames elided …]\n"
    ));
    for l in lines.iter().skip(total - ERROR_TAIL_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    out
}

// ── Fallback ──────────────────────────────────────────────────────────────

fn fallback_head_tail(content: &str, budget_chars: usize) -> String {
    let half = budget_chars / 2;
    let bytes = content.as_bytes();
    let head_end = char_boundary_before(content, half);
    let tail_start = char_boundary_after(content, bytes.len().saturating_sub(half));
    let elided_bytes = tail_start.saturating_sub(head_end);
    format!(
        "{}{COMPRESSION_MARKER}{elided_bytes} bytes elided …]{}",
        &content[..head_end],
        &content[tail_start..]
    )
}

fn char_boundary_before(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn char_boundary_after(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_under_budget_passes_through_unchanged() {
        let content = "short payload";
        assert_eq!(
            compress_result_for_context("t", content, 1000),
            content.to_string()
        );
    }

    #[test]
    fn large_json_array_is_compressed_with_head_and_tail() {
        let arr: Vec<i32> = (0..100).collect();
        let content = serde_json::to_string(&arr).unwrap();
        let out = compress_result_for_context("t", &content, 50);
        assert!(out.contains(COMPRESSION_MARKER), "got: {out}");
        // Head starts with [0, 1, 2, 3…
        assert!(out.starts_with("[0,1,2"));
        // Tail keeps the last few numbers (arr ends with …, 98, 99)
        assert!(out.contains("99"));
        assert!(out.len() < content.len());
    }

    #[test]
    fn small_json_array_not_compressed() {
        let arr: Vec<i32> = (0..3).collect();
        let content = serde_json::to_string(&arr).unwrap();
        // 13 chars; budget is 5 so it enters compression, but array is small
        // enough to be preserved intact inside the JSON compressor.
        let out = compress_result_for_context("t", &content, 5);
        assert_eq!(out, content);
    }

    #[test]
    fn large_json_object_drops_extra_keys_with_marker() {
        let mut obj = serde_json::Map::new();
        for i in 0..50 {
            obj.insert(format!("key{i:02}"), Value::from(i));
        }
        let content = Value::Object(obj).to_string();
        let out = compress_result_for_context("t", &content, 50);
        assert!(out.contains("__compressed__"), "got: {out}");
        assert!(out.contains("more keys elided"));
    }

    #[test]
    fn line_listing_compresses_middle_block() {
        let lines: Vec<String> = (0..200).map(|i| format!("line{i}")).collect();
        let content = lines.join("\n");
        let out = compress_result_for_context("grep", &content, 500);
        assert!(out.contains(COMPRESSION_MARKER));
        assert!(out.contains("line0\n"));
        assert!(out.contains("line199"));
        // Middle line should be gone.
        assert!(!out.contains("line100\n"));
    }

    #[test]
    fn error_payload_preserves_top_and_tail_frames() {
        let mut lines: Vec<String> =
            vec!["Error: something failed".into(), "Traceback (most recent call last):".into()];
        for i in 0..80 {
            lines.push(format!("  frame {i}: at somewhere"));
        }
        lines.push("RuntimeError: boom".into());
        let content = lines.join("\n");
        let out = compress_result_for_context("bash", &content, 200);
        assert!(out.contains("Error: something failed"));
        assert!(out.contains("RuntimeError: boom"));
        assert!(out.contains("middle frames elided"));
    }

    #[test]
    fn fallback_head_tail_keeps_char_boundaries() {
        // Pure text, no JSON, no listing, no error keyword. Use emoji
        // to force multi-byte boundaries.
        let content = "日本語".repeat(5000);
        let out = compress_result_for_context("t", &content, 200);
        assert!(out.contains(COMPRESSION_MARKER));
        // Should still round-trip as valid UTF-8.
        assert!(out.is_char_boundary(0));
    }

    #[test]
    fn non_json_non_listing_small_bytes_uses_fallback() {
        let content = "a".repeat(5000);
        let out = compress_result_for_context("t", &content, 100);
        assert!(out.contains(COMPRESSION_MARKER));
        assert!(out.len() < content.len());
    }

    #[test]
    fn default_budget_wrapper_uses_constant() {
        let content = "x".repeat(DEFAULT_COMPRESSION_BUDGET_CHARS + 100);
        let out = compress_with_default_budget("t", &content);
        assert!(out.len() < content.len());
    }

    #[test]
    fn is_line_listing_requires_many_lines() {
        assert!(!is_line_listing("a\nb\nc"));
        let many = (0..100).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        assert!(is_line_listing(&many));
    }

    #[test]
    fn looks_like_error_triggers_on_keywords() {
        assert!(looks_like_error("bash", "panic: oops"));
        assert!(looks_like_error("bash", "Traceback (most recent call last)"));
        assert!(looks_like_error("bash", "Exception: foo"));
        assert!(!looks_like_error("bash", "all good"));
    }

    #[test]
    fn looks_like_error_triggers_on_tool_name() {
        assert!(looks_like_error("error_reporter", "anything"));
    }

    #[test]
    fn json_array_compression_elision_count_correct() {
        let arr: Vec<i32> = (0..50).collect();
        let content = serde_json::to_string(&arr).unwrap();
        let out = compress_result_for_context("t", &content, 20);
        let expected_elided = 50 - JSON_ARRAY_HEAD - JSON_ARRAY_TAIL;
        assert!(
            out.contains(&format!("{expected_elided} array items elided")),
            "got: {out}"
        );
    }

    #[test]
    fn listing_elision_count_correct() {
        let lines: Vec<String> = (0..100).map(|i| format!("l{i}")).collect();
        let content = lines.join("\n");
        let out = compress_result_for_context("t", &content, 50);
        let expected = 100 - LISTING_HEAD_LINES - LISTING_TAIL_LINES;
        assert!(
            out.contains(&format!("{expected} middle lines elided")),
            "got: {out}"
        );
    }
}
