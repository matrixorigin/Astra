//! CLI output formatting utilities.
//!
//! Helper functions for formatting CLI output: truncation, path shortening,
//! byte sizes, durations, and diff colorization.

use crossterm::style::Stylize;
use serde_json::Value;
use std::borrow::Cow;

/// Unified diff for CLI summaries: `str_replace` / `multi_edit` sentinels, or `write_file` JSON field.
pub fn extract_cli_diff_block(output: &str) -> Option<Cow<'_, str>> {
    let start_marker = "<<<MO_AGENT_UNIFIED_DIFF>>>";
    let end_marker = "<<<END_MO_AGENT_UNIFIED_DIFF>>>";
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
pub fn colorize_diff_summary(diff: &str, max_lines: usize) -> String {
    let mut parts = Vec::new();
    let mut shown = 0usize;
    let mut total_add = 0usize;
    let mut total_del = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++ ") {
            total_add += 1;
            if shown < max_lines {
                parts.push(format!("{}", truncate_line(line, 60).green()));
                shown += 1;
            }
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            total_del += 1;
            if shown < max_lines {
                parts.push(format!("{}", truncate_line(line, 60).red()));
                shown += 1;
            }
        }
    }
    let remaining = (total_add + total_del).saturating_sub(max_lines);
    if remaining > 0 {
        parts.push(format!(
            "… {} {} (+{total_add} -{total_del} total)",
            format!("+{remaining}").dim(),
            "more".dim(),
        ));
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
/// Only shown for durations ≥ 1s. Returns e.g. " 3.2s", " 1m 4s", " 12m 30s".
pub fn format_duration_suffix(ms: u64) -> String {
    if ms < 1_000 {
        return String::new();
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

/// Shorten a path by keeping the filename and truncating dir prefix with "...".
pub fn shorten_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    // Keep the filename (last component)
    let parts: Vec<&str> = path.split('/').collect();
    if parts.is_empty() {
        return truncate_line(path, max_chars);
    }
    let filename = parts.last().unwrap_or(&"");
    if filename.chars().count() >= max_chars.saturating_sub(4) {
        // Filename itself is too long, just truncate
        return truncate_line(filename, max_chars);
    }
    // Try to keep one parent dir
    if parts.len() >= 2 {
        let parent = parts[parts.len() - 2];
        let short = format!(".../{parent}/{filename}");
        if short.chars().count() <= max_chars {
            return short;
        }
    }
    format!(".../{filename}")
}

/// Simple syntax highlighting for code preview.
/// Highlights: line numbers (dim), keywords (blue), strings (green), comments (dim green).
/// Works across multiple languages by using common keywords.
pub fn highlight_code_line(line: &str) -> String {
    use regex::Regex;

    // Common keywords across languages
    static KEYWORDS: &[&str] = &[
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

    // Check if line starts with line number (e.g., "420\t")
    let (prefix, code) = if let Some(tab_pos) = line.find('\t') {
        let num_part = &line[..tab_pos];
        if num_part
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_whitespace())
        {
            (format!("{}", num_part.dim()), &line[tab_pos..])
        } else {
            (String::new(), line)
        }
    } else {
        (String::new(), line)
    };

    // Detect comment start
    let comment_start = code.find("//").or_else(|| {
        code.find('#').filter(|&pos| {
            // Python/shell comments: # not inside string
            pos == 0 || code[..pos].matches('"').count() % 2 == 0
        })
    });

    let (code_part, comment_part) = if let Some(pos) = comment_start {
        (&code[..pos], Some(&code[pos..]))
    } else {
        (code, None)
    };

    // Build keyword regex
    let keyword_pattern = KEYWORDS.join("|");
    let keyword_re = Regex::new(&format!(r"\b({})\b", keyword_pattern)).unwrap();

    // Highlight keywords in code part
    let highlighted_code = keyword_re.replace_all(code_part, |caps: &regex::Captures| {
        format!("{}", caps[1].cyan())
    });

    // Highlight strings (simple: "..." or '...')
    let string_re = Regex::new(r#"("[^"]*"|'[^']*')"#).unwrap();
    let highlighted_code = string_re.replace_all(&highlighted_code, |caps: &regex::Captures| {
        format!("{}", caps[1].green())
    });

    // Combine parts
    let comment_highlighted = comment_part
        .map(|c| format!("{}", c.dim().green()))
        .unwrap_or_default();

    format!("{prefix}{highlighted_code}{comment_highlighted}")
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
        assert_eq!(format_duration_suffix(500), "");
        assert_eq!(format_duration_suffix(1000), " 1s");
        assert_eq!(format_duration_suffix(1500), " 1.5s");
        assert_eq!(format_duration_suffix(65000), " 1m 5s");
    }

    #[test]
    fn test_extract_cli_diff_block_sentinel() {
        let embedded = "+++ b/f\n+ok\n";
        let out = format!("<<<MO_AGENT_UNIFIED_DIFF>>>{embedded}<<<END_MO_AGENT_UNIFIED_DIFF>>>");
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
}
