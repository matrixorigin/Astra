//! CLI output formatting utilities.
//!
//! Helper functions for formatting CLI output: truncation, path shortening,
//! byte sizes, durations, and diff previews/colorization.

use crossterm::style::{Color, Stylize, style};
use serde_json::Value;
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

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
    if terminal_supports_truecolor() {
        let padded = pad_terminal_row(body);
        format!("{}", style(padded).with(truecolor_fg).on(truecolor_bg))
    } else {
        format!("{}", style(body).with(ansi_fg))
    }
}

fn render_terminal_diff_stat_row(body: String) -> String {
    if terminal_supports_truecolor() {
        format!(
            "{}",
            style(pad_terminal_row(body))
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
        format!("{}", style(body).with(Color::Cyan))
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

fn pad_terminal_row(mut text: String) -> String {
    let Ok((width, _)) = crossterm::terminal::size() else {
        return text;
    };
    let width = usize::from(width);
    let used = UnicodeWidthStr::width(text.as_str());
    if used < width {
        text.push_str(&" ".repeat(width - used));
    }
    text
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

// `truncate_line` is re-exported from `astra_text_utils::str_preview`
// at the top of this module (line 10). Keeping the definition here
// would force two copies to stay in sync — a recipe for silent drift
// between the scrollback preview and the approval-prompt preview.

/// Simple syntax highlighting for code preview.
/// Highlights: line numbers (dim), keywords (cyan), strings (green), comments (dim green).
/// Works across multiple languages by using common keywords.
pub fn highlight_code_line(line: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;

    // Cached regex patterns
    static KEYWORD_RE: OnceLock<Regex> = OnceLock::new();
    static STRING_RE: OnceLock<Regex> = OnceLock::new();

    // Common keywords across languages
    const KEYWORDS: &[&str] = &[
        "fn",
        "let",
        "mut",
        "const",
        "pub",
        "struct",
        "enum",
        "impl",
        "trait",
        "type",
        "use",
        "mod",
        "if",
        "else",
        "match",
        "for",
        "while",
        "loop",
        "return",
        "break",
        "continue",
        "async",
        "await",
        "self",
        "Self", // Rust
        "def",
        "class",
        "import",
        "from",
        "as",
        "pass",
        "None",
        "True",
        "False",
        "with", // Python
        "function",
        "var",
        "export",
        "default",
        "new",
        "this",
        "extends",
        "interface", // JS/TS
        "func",
        "package",
        "go",
        "defer",
        "chan",
        "select",
        "case", // Go
        "void",
        "int",
        "char",
        "float",
        "double",
        "bool",
        "true",
        "false",
        "null",
        "nullptr",
        "static",
        "final",
        "public",
        "private",
        "protected", // C/Java
    ];

    let keyword_re = KEYWORD_RE.get_or_init(|| {
        let pattern = KEYWORDS.join("|");
        Regex::new(&format!(r"\b({})\b", pattern)).expect("valid keyword regex")
    });

    // Match strings: "..." or '...' with basic escape handling (\")
    let string_re = STRING_RE.get_or_init(|| {
        Regex::new(r#"("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')"#).expect("valid regex")
    });

    // Check if line starts with line number (e.g., "420\t" or " 99\t")
    // Convert to more compact format: "  42│" (dim, right-aligned number + dim pipe)
    let (prefix, code) = if let Some(tab_pos) = line.find('\t') {
        let num_part = &line[..tab_pos];
        if num_part
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_whitespace())
        {
            // Right-align to 5 chars for files up to 99999 lines
            let num_trimmed = num_part.trim();
            let aligned = format!("{:>5}│", num_trimmed);
            (format!("{}", aligned.dim()), &line[tab_pos + 1..])
        } else {
            (String::new(), line)
        }
    } else {
        (String::new(), line)
    };

    // Find comment start, but not inside strings
    let comment_start = find_comment_start(code);

    let (code_part, comment_part) = if let Some(pos) = comment_start {
        (&code[..pos], Some(&code[pos..]))
    } else {
        (code, None)
    };

    // Highlight strings first (to avoid highlighting keywords inside strings)
    let highlighted_code = string_re.replace_all(code_part, |caps: &regex::Captures| {
        format!("{}", caps[1].green())
    });

    // Highlight keywords (but now strings are already colored, keywords inside won't match)
    let highlighted_code = keyword_re.replace_all(&highlighted_code, |caps: &regex::Captures| {
        format!("{}", caps[1].magenta())
    });

    // Combine parts
    let comment_highlighted = comment_part
        .map(|c| format!("{}", c.dim().green()))
        .unwrap_or_default();

    format!("{prefix}{highlighted_code}{comment_highlighted}")
}

/// Find comment start position, accounting for strings.
/// Returns None if no comment found, or Some(pos) for // or # comments.
fn find_comment_start(code: &str) -> Option<usize> {
    let mut in_string = false;
    let mut string_char = '\0';
    let mut escape_next = false;

    let mut chars = code.char_indices().peekable();
    while let Some((byte_pos, c)) = chars.next() {
        if escape_next {
            escape_next = false;
            continue;
        }

        if c == '\\' && in_string {
            escape_next = true;
            continue;
        }

        if !in_string {
            if c == '"' || c == '\'' {
                in_string = true;
                string_char = c;
            } else if c == '/' && chars.peek().map(|&(_, nc)| nc) == Some('/') {
                return Some(byte_pos);
            } else if c == '#' {
                // Python/shell comment
                return Some(byte_pos);
            }
        } else if c == string_char {
            in_string = false;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{
        colorize_diff_summary, colorize_git_diff_stat_summary, compact_unified_diff_preview,
        extract_cli_diff_block, find_comment_start, format_byte_size, format_duration_suffix,
        highlight_code_line, shorten_path, truncate_line,
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
        let stripped = strip_ansi(&rendered);
        let rows = stripped.lines().collect::<Vec<_>>();
        assert_eq!(rows[0].trim_end(), "    +21 -18 in 1 file(s)");
        assert!(rows[0].len() > rows[0].trim_end().len(), "{rows:?}");
        assert_eq!(rows[1], "          pkg/frontend/plan_cache.go");
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

    #[test]
    fn test_find_comment_start_simple() {
        assert_eq!(find_comment_start("let x = 1; // comment"), Some(11));
        assert_eq!(find_comment_start("# Python comment"), Some(0));
        assert_eq!(find_comment_start("no comment here"), None);
    }

    #[test]
    fn test_find_comment_start_in_string() {
        // // inside string should not be detected as comment
        assert_eq!(find_comment_start(r#"let s = "hello // world";"#), None);
        // # inside string should not be detected
        assert_eq!(find_comment_start(r#"x = "test # not comment""#), None);
    }

    #[test]
    fn test_find_comment_start_escaped_quotes() {
        // Escaped quote should not end string
        // `let s = "hello \"world\""; // real`
        // Position 25 is where `//` starts (0-indexed)
        assert_eq!(
            find_comment_start(r#"let s = "hello \"world\""; // real"#),
            Some(27)
        );
    }

    #[test]
    fn test_highlight_preserves_content() {
        // Highlighting should not lose characters
        let input = r#"let x = "hello"; // test"#;
        let output = highlight_code_line(input);
        // Should contain all original words (stripped of ANSI codes)
        let stripped = strip_ansi(&output);
        assert!(stripped.contains("let"));
        assert!(stripped.contains("hello"));
        assert!(stripped.contains("test"));
    }

    #[test]
    fn test_highlight_line_number_format() {
        // Line number should be right-aligned with pipe separator (5 chars)
        let input = "42\tlet x = 1;";
        let output = highlight_code_line(input);
        let stripped = strip_ansi(&output);
        // Should have right-aligned number and pipe: "   42│let x = 1;"
        assert!(stripped.starts_with("   42│"));
        assert!(stripped.contains("let x = 1;"));

        // Test with larger line number (fits in 5 chars)
        let input2 = "1234\tlet y = 2;";
        let output2 = highlight_code_line(input2);
        let stripped2 = strip_ansi(&output2);
        assert!(stripped2.starts_with(" 1234│"));

        // Test with 5-digit line number
        let input3 = "10000\tlet z = 3;";
        let output3 = highlight_code_line(input3);
        let stripped3 = strip_ansi(&output3);
        assert!(stripped3.starts_with("10000│"));
    }

    #[test]
    fn test_find_comment_start_multibyte() {
        // '#' after CJK chars — byte offset must be returned, not char index
        let s = "| 错误学习 | # comment";
        let pos = find_comment_start(s).unwrap();
        assert_eq!(&s[pos..pos + 1], "#");
    }

    #[test]
    fn test_highlight_code_line_multibyte() {
        // Must not panic on CJK content with comment-like characters
        let input = "| 错误学习 | 写入 `## Errors`（`[lesson]` 标签） |";
        let _output = highlight_code_line(input);
    }

    #[test]
    fn test_floor_char_boundary_truncation_safety() {
        // Regression: raw truncate at byte offset inside multi-byte char panics.
        // All truncation sites must use floor_char_boundary.
        let s = "abc你好def".to_string(); // 你 = bytes 3..6, 好 = bytes 6..9
        for n in 0..=s.len() {
            let boundary = s.floor_char_boundary(n);
            // Must not panic
            let _ = &s[..boundary];
        }
        // Verify it rounds down correctly
        assert_eq!(s.floor_char_boundary(4), 3); // inside '你', rounds to 3
        assert_eq!(s.floor_char_boundary(5), 3);
        assert_eq!(s.floor_char_boundary(6), 6); // exact boundary of '好'
    }

    /// Helper to strip ANSI escape codes for testing
    fn strip_ansi(s: &str) -> String {
        let re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        re.replace_all(s, "").to_string()
    }
}
