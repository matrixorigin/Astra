//! System-turn history cell — note / result / warning / error messages.
//!
//! Used for any TUI-generated notice that isn't model output:
//! session restored, permission mode changed, non-fatal errors,
//! etc. Rendered as compact labeled rows so they read like product
//! feedback rather than raw terminal logs.
//!
//! Persists as [`TurnEvent::System`]. Never live — the text is
//! fully known at construction time, same as `UserCell`.

use std::any::Any;

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::turn_event::{SystemLevel, TurnEvent};

#[derive(Debug, Clone)]
pub(crate) struct SystemCell {
    message: String,
    level: SystemLevel,
    presentation: SystemPresentation,
    ts: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemPresentation {
    Standard,
    BackgroundTask,
}

impl SystemCell {
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            level: SystemLevel::Info,
            presentation: SystemPresentation::Standard,
            ts: None,
        }
    }

    pub fn background_task(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            level: SystemLevel::Info,
            presentation: SystemPresentation::BackgroundTask,
            ts: None,
        }
    }

    /// Response to a slash command. Renders as a compact `Result ·`
    /// row so the reply visually pairs with the `> /cmd` line above:
    ///
    /// ```text
    /// > /model
    ///   Result · Set model to Opus 4.6
    /// ```
    pub fn response(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            level: SystemLevel::Response,
            presentation: SystemPresentation::Standard,
            ts: None,
        }
    }

    #[allow(dead_code)]
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            level: SystemLevel::Warning,
            presentation: SystemPresentation::Standard,
            ts: None,
        }
    }

    /// Error variant — runs the raw text through
    /// [`humanize_error`] to strip `<tool_use_error>` / `<error>`
    /// wrappers the agent layer sometimes emits and to cap the
    /// displayed length. The persisted copy is the humanised
    /// version, not the raw text, so resume renders the same as
    /// the live turn did.
    #[allow(dead_code)]
    pub fn error(raw: impl AsRef<str>) -> Self {
        Self {
            message: humanize_error(raw.as_ref()),
            level: SystemLevel::Error,
            presentation: SystemPresentation::Standard,
            ts: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_ts(mut self, ts: impl Into<String>) -> Self {
        self.ts = Some(ts.into());
        self
    }

    /// Resume constructor.
    #[allow(dead_code)]
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        match ev {
            TurnEvent::System { ts, level, text } => Some(Self {
                message: text,
                level,
                presentation: SystemPresentation::Standard,
                ts,
            }),
            _ => None,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn level(&self) -> SystemLevel {
        self.level
    }
}

impl HistoryCell for SystemCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let (prefix, label_style, body_style) = match self.presentation {
            SystemPresentation::BackgroundTask => (
                "↳ Background · ",
                Style::default().cyan().bold(),
                Style::default().fg(Color::White),
            ),
            SystemPresentation::Standard => match self.level {
                SystemLevel::Info => (
                    "ℹ Note · ",
                    Style::default()
                        .fg(crate::tui::theme::current().dim)
                        .add_modifier(ratatui::style::Modifier::DIM),
                    Style::default().fg(Color::Gray),
                ),
                SystemLevel::Response => (
                    "Result · ",
                    Style::default()
                        .fg(crate::tui::theme::current().dim)
                        .add_modifier(ratatui::style::Modifier::DIM),
                    Style::default().fg(Color::White),
                ),
                SystemLevel::Warning => (
                    "⚠ Warning · ",
                    Style::default().yellow().bold(),
                    Style::default().yellow(),
                ),
                SystemLevel::Error => (
                    "✖ Error · ",
                    Style::default().red().bold(),
                    Style::default().red(),
                ),
            },
        };
        let continuation = "  ".to_string();
        self.message
            .lines()
            .enumerate()
            .map(|(i, line)| {
                if i == 0 {
                    Line::from(vec![
                        Span::styled(prefix.to_string(), label_style),
                        Span::styled(line.to_string(), body_style),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(continuation.clone(), label_style),
                        Span::styled(line.to_string(), body_style),
                    ])
                }
            })
            .collect()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn to_persist(&self) -> Option<TurnEvent> {
        Some(TurnEvent::System {
            ts: self.ts.clone(),
            level: self.level,
            text: self.message.clone(),
        })
    }
}

/// Clean up raw tool/LLM error strings for display:
/// - strip `<tool_use_error>…</tool_use_error>` or `<error>…</error>`
///   wrappers the agent layer sometimes leaks into user-visible text,
/// - truncate to `MAX_BODY_LINES`, appending `… (+N more lines)` so
///   the user knows content was elided rather than silently dropped.
///
/// Split out as a free fn so it can be unit-tested independently of
/// the widget and reused by any future cell that surfaces raw error
/// text (tool failures, daemon errors, etc.).
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
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &SystemCell, width: u16, height: u16) -> String {
        let lines = cell.display_lines(width);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, height))
    }

    // ── Render ───────────────────────────────────────────────────

    #[test]
    fn info_renders_compact_feedback_row() {
        let cell = SystemCell::info("session resumed");
        let out = render(&cell, 40, 1);
        assert!(out.contains("session resumed"));
        assert!(out.starts_with("ℹ Note · "), "label missing: {out:?}");
    }

    #[test]
    fn response_gets_result_label_on_first_line() {
        // Slash-command response must visually pair with the `>
        // /cmd` prompt above it, with a stable `· Result` label.
        let cell = SystemCell::response("Set model to Opus 4.6");
        let out = render(&cell, 60, 1);
        assert!(
            out.contains("Result · "),
            "result label missing on response: {out:?}"
        );
        assert!(
            out.contains("Set model to Opus 4.6"),
            "body missing: {out:?}"
        );
    }

    #[test]
    fn background_task_renders_dedicated_label() {
        let cell =
            SystemCell::background_task("Opened bg-shell-1 details · S stop · Esc list · Q close");
        let out = render(&cell, 90, 1);

        assert!(out.starts_with("↳ Background · "), "label missing: {out:?}");
        assert!(out.contains("Opened bg-shell-1 details"), "{out}");
        assert!(!out.contains("Note ·"), "{out}");
    }

    #[test]
    fn response_continuation_lines_hang_indent() {
        // Multi-line responses must keep the label only on row 0 and
        // align continuation lines under the body so the block reads
        // as one unit.
        let cell = SystemCell::response("Set model to Opus 4.6\n(1M context, thinking enabled)");
        let out = render(&cell, 80, 2);
        let rows: Vec<&str> = out.lines().collect();
        assert!(
            rows[0].contains("Result · "),
            "first row should have Result label: {rows:?}"
        );
        assert!(
            !rows[1].contains("Result · "),
            "continuation must NOT repeat label: {rows:?}"
        );
        // Continuation rows should stay visually attached to the
        // response without drifting into a giant hanging indent.
        assert!(
            rows[1].starts_with("  "),
            "continuation row should use the compact follow-up indent: {rows:?}"
        );
    }

    #[test]
    fn multiline_rows_use_compact_follow_up_indent() {
        let cell = SystemCell::error("first line\nsecond line");
        let out = render(&cell, 40, 2);
        let rows: Vec<&str> = out.lines().collect();
        assert!(rows[0].starts_with("✖ Error · "), "{rows:?}");
        assert!(rows[1].starts_with("  "), "{rows:?}");
    }

    // ── Persistence ──────────────────────────────────────────────

    #[test]
    fn persist_roundtrip_for_each_level() {
        for (mk, lv) in [
            (SystemCell::info("a") as SystemCell, SystemLevel::Info),
            (SystemCell::background_task("bg"), SystemLevel::Info),
            (SystemCell::response("d"), SystemLevel::Response),
            (SystemCell::warning("b"), SystemLevel::Warning),
            (SystemCell::error("c"), SystemLevel::Error),
        ] {
            let persisted = mk.to_persist().unwrap();
            let back = SystemCell::from_persist(persisted).unwrap();
            assert_eq!(back.level(), lv);
        }
    }

    #[test]
    fn from_persist_rejects_wrong_variant() {
        let wrong = TurnEvent::User {
            ts: None,
            text: "x".into(),
        };
        assert!(SystemCell::from_persist(wrong).is_none());
    }

    #[test]
    fn error_persists_humanized_text_not_raw() {
        // Guards against a regression where `.error(raw)` would
        // store the raw `<tool_use_error>` wrapper on disk — resume
        // would then re-render the wrapper, stripped-off-by- the
        // humaniser at live time but not on reload.
        let cell = SystemCell::error("<error>rate limited</error>");
        let ev = cell.to_persist().unwrap();
        match ev {
            TurnEvent::System { text, .. } => {
                assert_eq!(text, "rate limited");
            }
            _ => panic!("wrong variant"),
        }
    }

    // ── humanize_error ───────────────────────────────────────────

    #[test]
    fn humanize_strips_tool_use_error() {
        assert_eq!(
            humanize_error("<tool_use_error>Unknown tool `foo`</tool_use_error>"),
            "Unknown tool `foo`"
        );
    }

    #[test]
    fn humanize_strips_error_with_whitespace() {
        assert_eq!(
            humanize_error("  <error>\n  rate limited\n  </error>  "),
            "rate limited"
        );
    }

    #[test]
    fn humanize_leaves_plain_error_alone() {
        assert_eq!(humanize_error("File not found"), "File not found");
    }

    #[test]
    fn humanize_truncates_long_body_with_marker() {
        let long: String = (0..25)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = humanize_error(&long);
        assert_eq!(out.lines().count(), 11, "10 kept + 1 marker");
        assert!(out.ends_with("… (+15 more lines)"));
    }

    #[test]
    fn humanize_short_bodies_no_marker() {
        let out = humanize_error("one\ntwo\nthree");
        assert_eq!(out, "one\ntwo\nthree");
        assert!(!out.contains("more lines"));
    }

    // ── Snapshots ────────────────────────────────────────────────

    #[test]
    fn snapshot_info_40() {
        crate::tui::testing::assert_tui_snapshot!(
            "system_info_40",
            render(&SystemCell::info("session resumed"), 40, 1)
        );
    }

    #[test]
    fn snapshot_response_60() {
        crate::tui::testing::assert_tui_snapshot!(
            "system_response_60",
            render(&SystemCell::response("Set model to Opus 4.6"), 60, 1)
        );
    }

    #[test]
    fn snapshot_response_multiline_80() {
        let cell = SystemCell::response("Set model to Opus 4.6\n(1M context, thinking enabled)");
        crate::tui::testing::assert_tui_snapshot!(
            "system_response_multiline_80",
            render(&cell, 80, 2)
        );
    }

    #[test]
    fn snapshot_warning_40() {
        crate::tui::testing::assert_tui_snapshot!(
            "system_warning_40",
            render(&SystemCell::warning("token budget 80%"), 40, 1)
        );
    }

    #[test]
    fn snapshot_error_multiline_60() {
        let cell = SystemCell::error("error: rate limited\nretry after 60s");
        crate::tui::testing::assert_tui_snapshot!(
            "system_error_multiline_60",
            render(&cell, 60, 2)
        );
    }
}
