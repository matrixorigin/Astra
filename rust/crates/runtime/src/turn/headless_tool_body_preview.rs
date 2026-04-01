//! Rich stderr previews for headless tool rounds (read body, unified diff).

use super::agentic_headless_round::{HeadlessRoundTerminal, HeadlessStderrStyle};
use super::tool_result_sanitize::{STR_REPLACE_DIFF_END, STR_REPLACE_DIFF_START};

const READ_PREVIEW_MAX_LINES: usize = 200;
const DIFF_EMIT_MAX_LINES: usize = 400;

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
            if !result_str.contains(STR_REPLACE_DIFF_START)
                && !result_str.contains("--- a/")
            {
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
