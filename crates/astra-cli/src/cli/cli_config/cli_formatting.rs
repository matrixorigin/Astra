//! CLI output formatting utilities.
//!
//! Helper functions for formatting CLI output: truncation, path shortening,
//! byte sizes, durations, and diff previews/colorization.

use crossterm::style::{Color, Stylize, style};
use serde_json::Value;
use std::borrow::Cow;

use crate::diff_utils::parse_hunk_header;

pub use astra_text_utils::str_preview::{shorten_path, truncate_line};

/// Unified diff for CLI summaries: `str_replace` / `multi_edit` sentinels, or `write_file` JSON field.
pub fn extract_cli_diff_block(output: &str) -> Option<Cow<'_, str>> {
    let start_marker = "<<<ASTRA_UNIFIED_DIFF>>>";
    let end_marker = "<<<END_ASTRA_UNIFIED_DIFF>>>";
    if let Some(start) = output.find(start_marker) {
        let after = &output[start + start_marker.len()..];
        let end = after.find(end_marker).unwrap_or(after.len());
        let block = after[..end].trim();
        if !block.is_empty() {
            return Some(Cow::Borrowed(block));
        }
    }
    let v = serde_json::from_str::<Value>(output.trim()).ok()?;
    let diff = v.get("_cli_unified_diff")?.as_str()?;
    if diff.is_empty() {
        return None;
    }
    Some(Cow::Owned(diff.to_string()))
}

const MAX_COLORIZED_DIFF_LINES: usize = 500;
const MAX_COLORIZED_DIFF_CHANGED_LINES: usize = 200;

/// Colorize a unified diff into a compact summary with green +lines and red -lines.
/// Shows context around changes for better understanding.
pub fn colorize_diff_summary(diff: &str) -> String {
    let owned_preview;
    let diff = if diff.lines().count() > MAX_COLORIZED_DIFF_LINES {
        owned_preview = compact_unified_diff_preview(diff, MAX_COLORIZED_DIFF_CHANGED_LINES);
        owned_preview.as_str()
    } else {
        diff
    };

    let mut parts = Vec::new();
    let mut old_line = 0u32;
    let mut new_line = 0u32;

    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some((old_start, new_start)) = parse_hunk_header(line) {
                old_line = old_start;
                new_line = new_start;
            }
            parts.push(format!("{}", line.cyan()));
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            let rendered = line
                .strip_prefix("--- a/")
                .or_else(|| line.strip_prefix("+++ b/"))
                .map(|path| shorten_path(path, 60))
                .unwrap_or_else(|| line.to_string());
            parts.push(format!("{}", rendered.dim().bold()));
            continue;
        }
        if let Some(code) = line.strip_prefix('+') {
            new_line += 1;
            let prefix = format!("{:>4} + ", new_line);
            let body = format!("{prefix}{code}");
            parts.push(render_terminal_diff_change(
                body,
                Color::Rgb {
                    r: 132,
                    g: 231,
                    b: 189,
                },
                Color::Rgb {
                    r: 19,
                    g: 49,
                    b: 40,
                },
                Color::DarkGreen,
            ));
            continue;
        }
        if let Some(code) = line.strip_prefix('-') {
            old_line += 1;
            let prefix = format!("{:>4} - ", old_line);
            let body = format!("{prefix}{code}");
            parts.push(render_terminal_diff_change(
                body,
                Color::Rgb {
                    r: 255,
                    g: 163,
                    b: 166,
                },
                Color::Rgb {
                    r: 59,
                    g: 33,
                    b: 39,
                },
                Color::DarkRed,
            ));
            continue;
        }
        if line.starts_with(' ') {
            old_line += 1;
            new_line += 1;
            let prefix = format!("{:>4}   ", new_line);
            parts.push(format!("{}{}", prefix.dim(), line[1..].dim()));
            continue;
        }
        parts.push(format!("{}", line.dim()));
    }

    parts.join("\n")
}

/// Render a compact `git diff` statistic as one neutral change surface.
///
/// A stat mixes additions and deletions, so colouring the whole row green or
/// red lies about its meaning. The surface is deliberately slate; the `+` and
/// `-` remain readable data rather than terminal escape sequences smuggled
/// into a summary string.
pub fn colorize_git_diff_stat_summary(summary: &str) -> String {
    summary
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 && looks_like_git_diff_stat(line) {
                render_terminal_diff_stat_row(format!("    {}", line.trim()))
            } else {
                format!("    {}", line.dim())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn looks_like_git_diff_stat(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('+') && line.contains(" -") && line.contains(" in ")
}

/// Render a changed row as one low-saturation semantic surface. The direct
/// streaming CLI cannot hand a typed [`ratatui::text::Line`] to the TUI, so it
/// must apply the same visual contract itself: on a truecolor terminal, paint
/// the entire physical row with the restrained edit surface; on weaker
/// terminals retain a readable foreground instead of falling back to a
/// fluorescent ANSI background.
fn render_terminal_diff_change(
    body: String,
    truecolor_fg: Color,
    truecolor_bg: Color,
    ansi_fg: Color,
) -> String {
    // EL executes while this row's style is active. Unlike width-sized space
    // padding, it works without a queryable TTY and never arms auto-wrap at
    // the terminal's final column.
    let row = format!("{body}\x1b[K");
    if terminal_supports_truecolor() {
        format!("{}", style(row).with(truecolor_fg).on(truecolor_bg))
    } else {
        format!("{}", style(row).with(ansi_fg))
    }
}

fn render_terminal_diff_stat_row(body: String) -> String {
    let row = format!("{body}\x1b[K");
    if terminal_supports_truecolor() {
        format!(
            "{}",
            style(row)
                .with(Color::Rgb {
                    r: 204,
                    g: 215,
                    b: 229,
                })
                .on(Color::Rgb {
                    r: 31,
                    g: 42,
                    b: 55,
                })
        )
    } else {
        format!("{}", style(row).with(Color::Cyan))
    }
}

fn terminal_supports_truecolor() -> bool {
    supports_color::on_cached(supports_color::Stream::Stderr).is_some_and(|level| level.has_16m)
        || std::env::var("COLORTERM")
            .map(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("truecolor") || value.contains("24bit")
            })
            .unwrap_or(false)
}

fn is_diff_change_line(line: &str) -> bool {
    (line.starts_with('+') && !line.starts_with("+++ "))
        || (line.starts_with('-') && !line.starts_with("--- "))
}

/// Build a compact unified-diff preview that keeps file/hunk headers plus the
/// first N changed lines, then appends an accurate folded-count marker.
pub fn compact_unified_diff_preview(diff: &str, max_changed_lines: usize) -> String {
    if max_changed_lines == 0 {
        return String::new();
    }

    let total_changed = diff
        .lines()
        .filter(|line| is_diff_change_line(line))
        .count();
    if total_changed == 0 {
        return String::new();
    }

    let mut preview = Vec::new();
    let mut pending_file_headers: Vec<&str> = Vec::new();
    let mut pending_hunk_header: Option<&str> = None;
    let mut file_headers_emitted = false;
    let mut hunk_header_emitted = false;
    let mut shown_changed = 0usize;

    for line in diff.lines() {
        if line.starts_with("diff --git ") || line.starts_with("index ") {
            continue;
        }

        if line.starts_with("--- ") {
            pending_file_headers.clear();
            pending_file_headers.push(line);
            pending_hunk_header = None;
            file_headers_emitted = false;
            hunk_header_emitted = false;
            continue;
        }

        if line.starts_with("+++ ") {
            pending_file_headers.push(line);
            file_headers_emitted = false;
            continue;
        }

        if line.starts_with("@@") {
            pending_hunk_header = Some(line);
            hunk_header_emitted = false;
            continue;
        }

        if !is_diff_change_line(line) {
            continue;
        }

        if shown_changed >= max_changed_lines {
            continue;
        }

        if !file_headers_emitted {
            preview.extend(pending_file_headers.iter().map(|line| (*line).to_string()));
            file_headers_emitted = true;
        }
        if !hunk_header_emitted {
            if let Some(header) = pending_hunk_header {
                preview.push(header.to_string());
            }
            hunk_header_emitted = true;
        }

        preview.push(line.to_string());
        shown_changed += 1;
    }

    let remaining = total_changed.saturating_sub(shown_changed);
    if remaining > 0 {
        preview.push(format!("… +{remaining} more changed lines"));
    }

    preview.join("\n")
}

/// Format a byte count at human scale (B, KiB, MiB, GiB).
pub fn format_byte_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Format duration as a human-friendly suffix for the tool description line.
/// Returns e.g. " 42ms", " 3.2s", " 1m 4s", " 12m 30s".
pub fn format_duration_suffix(ms: u64) -> String {
    if ms < 1_000 {
        return format!(" {ms}ms");
    }
    let secs = ms / 1_000;
    if secs < 60 {
        let frac = (ms % 1_000) / 100;
        if frac > 0 {
            format!(" {secs}.{frac}s")
        } else {
            format!(" {secs}s")
        }
    } else {
        let m = secs / 60;
        let s = secs % 60;
        if s > 0 {
            format!(" {m}m {s}s")
        } else {
            format!(" {m}m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        colorize_diff_summary, colorize_git_diff_stat_summary, compact_unified_diff_preview,
        extract_cli_diff_block, format_byte_size, format_duration_suffix, shorten_path,
        truncate_line,
    };

    #[test]
    fn test_truncate_line() {
        assert_eq!(truncate_line("hello", 10), "hello");
        assert_eq!(truncate_line("hello world", 5), "hell…");
        assert_eq!(truncate_line("line1\nline2", 20), "line1");
    }

    #[test]
    fn test_shorten_path() {
        assert_eq!(shorten_path("short.txt", 20), "short.txt");
        // "/a/b/c/d/e/file.txt" is 19 chars, use max_chars=15 to trigger shortening
        assert_eq!(shorten_path("/a/b/c/d/e/file.txt", 15), ".../e/file.txt");
        // When filename is too long relative to max_chars, it gets truncated directly
        assert_eq!(shorten_path("/a/very_long_filename.txt", 10), "very_long…");
        // When there's room for .../parent/filename (16 chars: "/a/b/c/short.txt")
        assert_eq!(shorten_path("/a/b/c/short.txt", 14), ".../short.txt");
    }

    #[test]
    fn test_format_byte_size() {
        assert_eq!(format_byte_size(100), "100B");
        assert_eq!(format_byte_size(1024), "1.0KB");
        assert_eq!(format_byte_size(1024 * 1024), "1.0MB");
    }

    #[test]
    fn test_format_duration_suffix() {
        assert_eq!(format_duration_suffix(42), " 42ms");
        assert_eq!(format_duration_suffix(500), " 500ms");
        assert_eq!(format_duration_suffix(1000), " 1s");
        assert_eq!(format_duration_suffix(1500), " 1.5s");
        assert_eq!(format_duration_suffix(65000), " 1m 5s");
    }

    #[test]
    fn test_extract_cli_diff_block_sentinel() {
        let embedded = "+++ b/f\n+ok\n";
        let out = format!("<<<ASTRA_UNIFIED_DIFF>>>{embedded}<<<END_ASTRA_UNIFIED_DIFF>>>");
        let got = extract_cli_diff_block(&out).expect("diff");
        assert_eq!(got.as_ref(), embedded.trim());
    }

    #[test]
    fn test_extract_cli_diff_block_json() {
        let diff_body = "--- a/x.js\n+++ b/x.js\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let out = serde_json::json!({
            "success": true,
            "bytes_written": 3u32,
            "path": "/tmp/x.js",
            "_cli_unified_diff": diff_body,
        })
        .to_string();
        let got = extract_cli_diff_block(&out).expect("diff");
        assert_eq!(got.as_ref(), diff_body);
    }

    #[test]
    fn compact_unified_diff_preview_keeps_headers_and_correct_fold_count() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -10,3 +10,4 @@\n\
-old1\n\
+new1\n\
-old2\n\
+new2\n\
+new3\n";
        let preview = compact_unified_diff_preview(diff, 3);
        assert_eq!(
            preview,
            "\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -10,3 +10,4 @@\n\
-old1\n\
+new1\n\
-old2\n\
… +2 more changed lines"
        );
    }

    #[test]
    fn colorize_diff_summary_renders_line_numbers_from_hunks() {
        let preview = "\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -41,2 +41,2 @@\n\
-old\n\
+new";
        let rendered = colorize_diff_summary(preview);
        let stripped = strip_ansi(&rendered);
        assert!(stripped.contains("src/a.rs"));
        assert!(stripped.contains("@@ -41,2 +41,2 @@"));
        assert!(stripped.contains("  41 - old"), "{stripped}");
        assert!(stripped.contains("  41 + new"), "{stripped}");
    }

    #[test]
    fn colorize_diff_summary_hard_caps_large_input_in_release_builds() {
        let mut diff = String::from("--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,300 @@\n");
        for i in 0..600 {
            diff.push_str(&format!("+line-{i}\n"));
        }

        let rendered = colorize_diff_summary(&diff);
        let stripped = strip_ansi(&rendered);
        assert!(stripped.contains("… +400 more changed lines"), "{stripped}");
        assert!(!stripped.contains("line-599"), "{stripped}");
    }

    #[test]
    fn git_diff_stat_uses_a_neutral_diff_surface_not_a_success_colour() {
        let rendered = colorize_git_diff_stat_summary(
            "+21 -18 in 1 file(s)\n      pkg/frontend/plan_cache.go",
        );
        // `strip_ansi` intentionally strips SGR styling only. EL is a
        // terminal geometry command, so remove it separately when asserting
        // visible cells while retaining the explicit geometry assertion below.
        let visible = rendered.replace("\x1b[K", "");
        let stripped = strip_ansi(&visible);
        let rows = stripped.lines().collect::<Vec<_>>();
        assert_eq!(rows[0], "    +21 -18 in 1 file(s)");
        assert_eq!(rows[1], "          pkg/frontend/plan_cache.go");
        assert!(
            rendered
                .lines()
                .next()
                .unwrap_or_default()
                .contains("\x1b[K"),
            "the neutral stat surface must erase through the physical row edge: {rendered:?}"
        );
    }

    #[test]
    fn compact_unified_diff_preview_handles_multiple_files() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1,1 +1,1 @@\n\
-old-a\n\
+new-a\n\
diff --git a/src/b.rs b/src/b.rs\n\
--- a/src/b.rs\n\
+++ b/src/b.rs\n\
@@ -10,1 +10,2 @@\n\
-old-b\n\
+new-b\n\
+new-b2\n";
        let preview = compact_unified_diff_preview(diff, 3);
        assert!(preview.contains("--- a/src/a.rs"));
        assert!(preview.contains("+++ b/src/a.rs"));
        assert!(preview.contains("--- a/src/b.rs"));
        assert!(preview.contains("+++ b/src/b.rs"));
        assert!(preview.contains("-old-b"));
        assert!(preview.contains("… +2 more changed lines"));
    }

    /// Helper to strip ANSI escape codes for testing
    fn strip_ansi(s: &str) -> String {
        let re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        re.replace_all(s, "").to_string()
    }
}
