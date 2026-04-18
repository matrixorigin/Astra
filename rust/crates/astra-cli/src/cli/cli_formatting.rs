//! CLI output formatting utilities.
//!
//! Helper functions for formatting CLI output: truncation, path shortening,
//! byte sizes, durations, and diff colorization.

use crossterm::style::Stylize;
use serde_json::Value;
use std::borrow::Cow;

pub use astra_text_utils::str_preview::{github_repo_display, shorten_path};

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

/// Colorize a unified diff into a compact summary with green +lines and red -lines.
/// Shows context around changes for better understanding.
pub fn colorize_diff_summary(diff: &str, max_lines: usize) -> String {
    let mut parts = Vec::new();
    let mut shown = 0usize;
    let mut total_add = 0usize;
    let mut total_del = 0usize;

    // Extract the file path from diff header
    let mut file_path: Option<&str> = None;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            file_path = Some(path);
            break;
        }
    }

    // Add file header if found
    if let Some(path) = file_path {
        let short = shorten_path(path, 50);
        parts.push(format!("{}", short.dim()));
    }

    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++ ") {
            total_add += 1;
            if shown < max_lines {
                // Green + with highlighted code
                let code = &line[1..]; // Skip the '+' prefix
                let highlighted = highlight_code_line(code);
                parts.push(format!("{}{}", "+".green(), highlighted));
                shown += 1;
            }
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            total_del += 1;
            if shown < max_lines {
                // Red - with dimmed code (deleted)
                let code = &line[1..]; // Skip the '-' prefix
                parts.push(format!("{}{}", "-".red(), code.dim()));
                shown += 1;
            }
        }
    }
    let remaining = (total_add + total_del).saturating_sub(max_lines);
    if remaining > 0 {
        parts.push(format!(
            "{}",
            format!("… +{remaining} more ({total_add}+ {total_del}-)").dim(),
        ));
    } else if total_add > 0 || total_del > 0 {
        // Show total counts on the last line
        parts.push(format!("{}", format!("{total_add}+ {total_del}-").dim(),));
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.join("\n    ")
}

/// Format byte size as human-friendly string.
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

/// Truncate a string to max_chars, adding "…" if truncated.
pub fn truncate_line(s: &str, max_chars: usize) -> String {
    // Take first line only
    let line = s.lines().next().unwrap_or(s);
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

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
        format!("{}", caps[1].cyan())
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
    use super::*;

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
