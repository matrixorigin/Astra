//! Rich stderr previews for headless tool rounds (read body, unified diff).

use super::agentic_headless_round::{HeadlessRoundTerminal, HeadlessStderrStyle};
use super::tool_result_sanitize::{STR_REPLACE_DIFF_END, STR_REPLACE_DIFF_START};

/// Tighter than before — large reviews should not flood stderr; use `/diff` or open files.
const READ_PREVIEW_MAX_LINES: usize = 72;
const DIFF_EMIT_MAX_LINES: usize = 120;

/// After tool OK/error headers, emit read bodies and diffs (no-op when `quiet` or error).
pub fn emit_headless_tool_body_preview(
    term: &mut dyn HeadlessRoundTerminal,
    quiet: bool,
    tool_name: &str,
    result_str: &str,
    is_err: bool,
) {
    if quiet || is_err {
        return;
    }
    match tool_name {
        "read_file" => emit_read_file_preview(term, result_str),
        "write_file" => emit_write_file_diff_preview(term, result_str),
        "str_replace" => emit_str_replace_or_dry_run_diff_preview(term, result_str),
        "git_diff" => emit_plain_diffish_preview(term, result_str),
        "multi_edit" => {
            emit_str_replace_or_dry_run_diff_preview(term, result_str);
            if !result_str.contains(STR_REPLACE_DIFF_START) && !result_str.contains("--- a/") {
                emit_plain_diffish_preview(term, result_str);
            }
        }
        _ => {}
    }
}

fn emit_read_file_preview(term: &mut dyn HeadlessRoundTerminal, result: &str) {
    if result.starts_with("data:image/") && result.contains(";base64,") {
        term.emit_line(
            HeadlessStderrStyle::CyanBold,
            "── binary image payload omitted (base64) ──".to_string(),
        );
        return;
    }
    let lines: Vec<&str> = result.lines().collect();
    let total = lines.len();
    let take = total.min(READ_PREVIEW_MAX_LINES);
    if take == 0 {
        return;
    }
    term.emit_line(
        HeadlessStderrStyle::CyanBold,
        "── read_file ─────────────────────────────────────────".to_string(),
    );
    for line in &lines[..take] {
        term.emit_line(HeadlessStderrStyle::Normal, (*line).to_string());
    }
    if total > READ_PREVIEW_MAX_LINES {
        term.emit_line(
            HeadlessStderrStyle::Dim,
            format!("… {total} lines total, showing first {READ_PREVIEW_MAX_LINES} …"),
        );
    }
}

fn emit_write_file_diff_preview(term: &mut dyn HeadlessRoundTerminal, json_str: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return;
    };
    let Some(diff) = v.get("_cli_unified_diff").and_then(|x| x.as_str()) else {
        return;
    };
    term.emit_line(
        HeadlessStderrStyle::CyanBold,
        "── write_file (unified diff) ──────────────────────────".to_string(),
    );
    emit_unified_diff_lines(term, diff);
}

/// `str_replace` / `multi_edit`: sentinel-wrapped diff, or `[DRY RUN]` unified diff body.
fn emit_str_replace_or_dry_run_diff_preview(term: &mut dyn HeadlessRoundTerminal, result: &str) {
    if let Some(diff) = extract_sentinel_unified_diff(result) {
        term.emit_line(
            HeadlessStderrStyle::CyanBold,
            "── unified diff ────────────────────────────────────────".to_string(),
        );
        emit_unified_diff_lines(term, diff);
        return;
    }
    if let Some(diff) = extract_dry_run_unified_diff(result) {
        term.emit_line(
            HeadlessStderrStyle::CyanBold,
            "── dry run (unified diff) ────────────────────────────".to_string(),
        );
        emit_unified_diff_lines(term, diff);
    }
}

fn extract_sentinel_unified_diff(result: &str) -> Option<&str> {
    let start = result.find(STR_REPLACE_DIFF_START)?;
    let after = &result[start + STR_REPLACE_DIFF_START.len()..];
    let end_rel = after.find(STR_REPLACE_DIFF_END)?;
    Some(&after[..end_rel])
}

/// Strip `[DRY RUN] ...` prefix and return unified diff starting at `--- a/`.
fn extract_dry_run_unified_diff(result: &str) -> Option<&str> {
    let idx = result.find("--- a/")?;
    Some(&result[idx..])
}

fn emit_plain_diffish_preview(term: &mut dyn HeadlessRoundTerminal, result: &str) {
    if result.contains("--- ") && (result.contains("+++ ") || result.contains("+++")) {
        term.emit_line(
            HeadlessStderrStyle::CyanBold,
            "── diff ────────────────────────────────────────────────".to_string(),
        );
        emit_unified_diff_lines(term, result);
        return;
    }
    let lines: Vec<&str> = result.lines().collect();
    let take = lines.len().min(READ_PREVIEW_MAX_LINES);
    if take == 0 {
        return;
    }
    term.emit_line(
        HeadlessStderrStyle::CyanBold,
        "── output ─────────────────────────────────────────────".to_string(),
    );
    for line in &lines[..take] {
        term.emit_line(HeadlessStderrStyle::Normal, (*line).to_string());
    }
}

fn emit_unified_diff_lines(term: &mut dyn HeadlessRoundTerminal, diff: &str) {
    let lines: Vec<&str> = diff.lines().collect();
    let total = lines.len();
    let take = total.min(DIFF_EMIT_MAX_LINES);
    for line in &lines[..take] {
        let style = diff_line_style(line);
        term.emit_line(style, (*line).to_string());
    }
    if total > DIFF_EMIT_MAX_LINES {
        term.emit_line(
            HeadlessStderrStyle::Dim,
            format!("… diff truncated ({total} lines, showing {DIFF_EMIT_MAX_LINES}) …"),
        );
    }
}

fn diff_line_style(line: &str) -> HeadlessStderrStyle {
    if line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("diff --git ")
        || line.starts_with("Binary files ")
    {
        return HeadlessStderrStyle::CyanBold;
    }
    if line.starts_with("@@") {
        return HeadlessStderrStyle::Magenta;
    }
    if line.starts_with('\\') {
        return HeadlessStderrStyle::DiffContext;
    }
    if line.starts_with(' ') {
        return HeadlessStderrStyle::DiffContext;
    }
    if let Some(rest) = line.strip_prefix('-') {
        if rest.starts_with('-') {
            return HeadlessStderrStyle::CyanBold;
        }
        return HeadlessStderrStyle::DiffRemove;
    }
    if let Some(rest) = line.strip_prefix('+') {
        if rest.starts_with('+') {
            return HeadlessStderrStyle::CyanBold;
        }
        return HeadlessStderrStyle::DiffAdd;
    }
    HeadlessStderrStyle::Normal
}

#[cfg(test)]
mod tests {
    use super::*;

    // ──────────────────────────────────────────────────────────
    // extract_sentinel_unified_diff
    // ──────────────────────────────────────────────────────────

    #[test]
    fn sentinel_diff_extracts_content() {
        let input = format!(
            "OK applied.{}--- a/file.rs\n+++ b/file.rs\n@@ -1 +1 @@\n-old\n+new{}trailing",
            STR_REPLACE_DIFF_START, STR_REPLACE_DIFF_END
        );
        let diff = extract_sentinel_unified_diff(&input).unwrap();
        assert!(diff.contains("--- a/file.rs"));
        assert!(diff.contains("+new"));
    }

    #[test]
    fn sentinel_diff_no_start_marker() {
        assert_eq!(extract_sentinel_unified_diff("no markers here"), None);
    }

    #[test]
    fn sentinel_diff_no_end_marker() {
        let input = format!("text{}diff content but no end", STR_REPLACE_DIFF_START);
        assert_eq!(extract_sentinel_unified_diff(&input), None);
    }

    #[test]
    fn sentinel_diff_empty_between_markers() {
        let input = format!("{}{}", STR_REPLACE_DIFF_START, STR_REPLACE_DIFF_END);
        let diff = extract_sentinel_unified_diff(&input).unwrap();
        assert!(diff.is_empty());
    }

    // ──────────────────────────────────────────────────────────
    // extract_dry_run_unified_diff
    // ──────────────────────────────────────────────────────────

    #[test]
    fn dry_run_diff_extracts_from_marker() {
        let input = "[DRY RUN] Would change file.rs\n--- a/file.rs\n+++ b/file.rs\n-old\n+new";
        let diff = extract_dry_run_unified_diff(input).unwrap();
        assert!(diff.starts_with("--- a/file.rs"));
    }

    #[test]
    fn dry_run_diff_no_marker() {
        assert_eq!(extract_dry_run_unified_diff("no diff here"), None);
    }

    // ──────────────────────────────────────────────────────────
    // diff_line_style
    // ──────────────────────────────────────────────────────────

    #[test]
    fn style_diff_header_lines() {
        assert!(matches!(diff_line_style("--- a/file.rs"), HeadlessStderrStyle::CyanBold));
        assert!(matches!(diff_line_style("+++ b/file.rs"), HeadlessStderrStyle::CyanBold));
        assert!(matches!(diff_line_style("diff --git a/f b/f"), HeadlessStderrStyle::CyanBold));
        assert!(matches!(diff_line_style("Binary files differ"), HeadlessStderrStyle::CyanBold));
    }

    #[test]
    fn style_hunk_header() {
        assert!(matches!(diff_line_style("@@ -1,3 +1,4 @@"), HeadlessStderrStyle::Magenta));
    }

    #[test]
    fn style_context_lines() {
        assert!(matches!(diff_line_style(" unchanged line"), HeadlessStderrStyle::DiffContext));
        assert!(matches!(diff_line_style("\\ No newline at end of file"), HeadlessStderrStyle::DiffContext));
    }

    #[test]
    fn style_add_remove_lines() {
        assert!(matches!(diff_line_style("+added line"), HeadlessStderrStyle::DiffAdd));
        assert!(matches!(diff_line_style("-removed line"), HeadlessStderrStyle::DiffRemove));
    }

    #[test]
    fn style_double_prefix_is_header() {
        // "---" and "+++" with rest starting with same char → CyanBold
        assert!(matches!(diff_line_style("--some line"), HeadlessStderrStyle::CyanBold));
        assert!(matches!(diff_line_style("++some line"), HeadlessStderrStyle::CyanBold));
    }

    #[test]
    fn style_normal_line() {
        assert!(matches!(diff_line_style("normal text"), HeadlessStderrStyle::Normal));
        assert!(matches!(diff_line_style(""), HeadlessStderrStyle::Normal));
    }

    // ──────────────────────────────────────────────────────────
    // emit_headless_tool_body_preview (integration via mock)
    // ──────────────────────────────────────────────────────────

    struct MockTerminal {
        lines: Vec<(HeadlessStderrStyle, String)>,
    }

    impl MockTerminal {
        fn new() -> Self {
            Self { lines: Vec::new() }
        }
    }

    impl HeadlessRoundTerminal for MockTerminal {
        fn emit_line(&mut self, style: HeadlessStderrStyle, text: String) {
            self.lines.push((style, text));
        }
    }

    #[test]
    fn preview_quiet_emits_nothing() {
        let mut term = MockTerminal::new();
        emit_headless_tool_body_preview(&mut term, true, "read_file", "content", false);
        assert!(term.lines.is_empty());
    }

    #[test]
    fn preview_error_emits_nothing() {
        let mut term = MockTerminal::new();
        emit_headless_tool_body_preview(&mut term, false, "read_file", "error text", true);
        assert!(term.lines.is_empty());
    }

    #[test]
    fn preview_read_file_emits_content() {
        let mut term = MockTerminal::new();
        emit_headless_tool_body_preview(&mut term, false, "read_file", "line1\nline2", false);
        assert!(!term.lines.is_empty());
        assert!(term.lines[0].1.contains("read_file"));
    }

    #[test]
    fn preview_read_file_base64_image() {
        let mut term = MockTerminal::new();
        emit_headless_tool_body_preview(
            &mut term,
            false,
            "read_file",
            "data:image/png;base64,abc123",
            false,
        );
        assert!(!term.lines.is_empty());
        assert!(term.lines[0].1.contains("binary image"));
    }

    #[test]
    fn preview_read_file_empty_result() {
        let mut term = MockTerminal::new();
        emit_headless_tool_body_preview(&mut term, false, "read_file", "", false);
        assert!(term.lines.is_empty());
    }

    #[test]
    fn preview_unknown_tool_emits_nothing() {
        let mut term = MockTerminal::new();
        emit_headless_tool_body_preview(&mut term, false, "unknown_tool", "output", false);
        assert!(term.lines.is_empty());
    }

    #[test]
    fn preview_str_replace_with_sentinel_diff() {
        let mut term = MockTerminal::new();
        let result = format!(
            "OK{}--- a/f.rs\n+++ b/f.rs\n-old\n+new{}",
            STR_REPLACE_DIFF_START, STR_REPLACE_DIFF_END
        );
        emit_headless_tool_body_preview(&mut term, false, "str_replace", &result, false);
        assert!(!term.lines.is_empty());
        assert!(term.lines[0].1.contains("unified diff"));
    }
}
