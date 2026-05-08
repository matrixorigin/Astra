use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use super::ChatCell;

#[derive(Debug)]
pub(crate) struct SystemChatCell {
    pub message: String,
    pub level: SystemLevel,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SystemLevel {
    Info,
    #[allow(dead_code)]
    Warning,
    #[allow(dead_code)]
    Error,
}

impl SystemChatCell {
    pub fn info(message: String) -> Self {
        Self {
            message,
            level: SystemLevel::Info,
        }
    }

    #[allow(dead_code)]
    pub fn warning(message: String) -> Self {
        Self {
            message,
            level: SystemLevel::Warning,
        }
    }

    #[allow(dead_code)]
    pub fn error(message: String) -> Self {
        Self {
            message: humanize_error(&message),
            level: SystemLevel::Error,
        }
    }
}

/// Clean up raw tool/LLM error strings for display in a system cell:
/// - strip common `<tool_use_error>...</tool_use_error>` /
///   `<error>...</error>` wrappers that leak from the agent layer.
/// - truncate to at most `MAX_BODY_LINES` lines, appending a
///   `… (+N more lines)` marker so users see something's there.
///
/// Pulled out as a free fn so it's easy to test independently.
pub(crate) fn humanize_error(raw: &str) -> String {
    const MAX_BODY_LINES: usize = 10;

    let trimmed = raw.trim();
    let stripped = strip_wrapping_tag(trimmed, "tool_use_error")
        .or_else(|| strip_wrapping_tag(trimmed, "error"))
        .unwrap_or_else(|| trimmed.to_string());

    let stripped = stripped.trim();
    let lines: Vec<&str> = stripped.lines().collect();
    if lines.len() <= MAX_BODY_LINES {
        return stripped.to_string();
    }
    let keep = &lines[..MAX_BODY_LINES];
    let rest = lines.len() - MAX_BODY_LINES;
    let mut out: String = keep.join("\n");
    out.push_str(&format!("\n… (+{rest} more lines)"));
    out
}

/// Return the inner content if `s` looks like `<tag>…</tag>`, else `None`.
fn strip_wrapping_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = s.trim();
    let start = s.find(&open)?;
    let end = s.rfind(&close)?;
    if start >= end {
        return None;
    }
    let inner = &s[start + open.len()..end];
    Some(inner.trim().to_string())
}

#[cfg(test)]
mod humanize_tests {
    use super::*;

    #[test]
    fn strips_tool_use_error_wrapper() {
        let raw = "<tool_use_error>Unknown tool `foo`</tool_use_error>";
        assert_eq!(humanize_error(raw), "Unknown tool `foo`");
    }

    #[test]
    fn strips_error_wrapper_with_whitespace() {
        let raw = "  <error>\n  rate limited\n  </error>  ";
        assert_eq!(humanize_error(raw), "rate limited");
    }

    #[test]
    fn leaves_plain_error_alone() {
        assert_eq!(humanize_error("File not found"), "File not found");
    }

    #[test]
    fn truncates_body_to_10_lines_with_marker() {
        let long: String = (0..25)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = humanize_error(&long);
        let line_count = out.lines().count();
        assert_eq!(
            line_count,
            11,
            "10 kept lines + one truncation marker; got {line_count} lines"
        );
        assert!(
            out.ends_with("… (+15 more lines)"),
            "marker missing; got: {out}"
        );
    }

    #[test]
    fn short_errors_dont_add_marker() {
        let raw = "one\ntwo\nthree";
        assert_eq!(humanize_error(raw), "one\ntwo\nthree");
        assert!(!humanize_error(raw).contains("more lines"));
    }

    #[test]
    fn strip_then_truncate_composes() {
        let inner = (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let raw = format!("<tool_use_error>\n{inner}\n</tool_use_error>");
        let out = humanize_error(&raw);
        // 10 kept + marker
        assert_eq!(out.lines().count(), 11);
        assert!(out.starts_with("line 0"));
        assert!(out.ends_with("… (+10 more lines)"));
    }
}

impl ChatCell for SystemChatCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn as_any_ref(&self) -> &dyn std::any::Any {
        self
    }
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let style = match self.level {
            SystemLevel::Info => Style::default().dim(),
            SystemLevel::Warning => Style::default().yellow(),
            SystemLevel::Error => Style::default().red(),
        };
        self.message
            .lines()
            .map(|l| Line::from(Span::styled(format!("  {l}"), style)))
            .collect()
    }
}
