use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

use super::ChatCell;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolStatus {
    Running,
    Success,
    Failed,
}

#[derive(Debug)]
pub(crate) struct ToolChatCell {
    pub name: String,
    pub description: String,
    pub status: ToolStatus,
    pub started_at: Instant,
    pub duration_ms: Option<u64>,
    pub output_summary: Option<String>,
    pub output: Option<String>,
}

impl ToolChatCell {
    pub fn new_running(name: String, description: String) -> Self {
        Self {
            name,
            description,
            status: ToolStatus::Running,
            started_at: Instant::now(),
            duration_ms: None,
            output_summary: None,
            output: None,
        }
    }

    pub fn complete(
        &mut self,
        status_str: &str,
        duration_ms: u64,
        description: String,
        output_summary: Option<String>,
        output: Option<String>,
    ) {
        self.status = if status_str == "success" {
            ToolStatus::Success
        } else {
            ToolStatus::Failed
        };
        self.duration_ms = Some(duration_ms);
        if !description.is_empty() {
            self.description = description;
        }
        self.output_summary = output_summary;
        self.output = output;
    }

    fn bullet(&self) -> Span<'static> {
        match self.status {
            // Running uses the theme accent so the in-progress row
            // pops from the dim scrollback — users reported the dim
            // bullet was invisible on their terminal, making fast tool
            // calls look like they skipped the "running" phase entirely.
            ToolStatus::Running => {
                let theme = crate::tui::theme::current();
                Span::styled("• ", Style::default().fg(theme.accent).bold())
            }
            ToolStatus::Success => Span::styled("• ", Style::default().fg(Color::Green).bold()),
            ToolStatus::Failed => Span::styled("• ", Style::default().fg(Color::Red).bold()),
        }
    }

    fn elapsed_str(&self) -> String {
        if let Some(ms) = self.duration_ms {
            if ms < 1000 {
                format!("{ms}ms")
            } else {
                format!("{:.1}s", ms as f64 / 1000.0)
            }
        } else {
            let ms = self.started_at.elapsed().as_millis();
            if ms < 1000 {
                format!("{ms}ms")
            } else {
                format!("{:.1}s", ms as f64 / 1000.0)
            }
        }
    }

    /// A one-row progress line rendered under the header while a tool
    /// is running past the 3s threshold. Uses a Braille spinner (time-
    /// driven phase) plus a logarithmic fill bar so the user sees
    /// activity even when the underlying work offers no real progress
    /// metric.
    fn progress_line(&self, width: usize, elapsed_ms: u64) -> Line<'static> {
        const FRAMES: [&str; 10] = [
            "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
        ];
        let frame_idx = ((elapsed_ms / 80) % FRAMES.len() as u64) as usize;
        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(ratatui::style::Color::DarkGray);

        // Logarithmic bar: 0 at t=0, ~86% at t=30s, asymptotic but
        // capped below 100% so the user can always tell a long-running
        // tool apart from a finished one.
        let raw = 1.0 - (-(elapsed_ms as f32 / 15_000.0)).exp();
        let progress = raw.min(0.99);
        // Bar area: gutter(4) + spinner(2) + " " + bar ...  leave 8 slack.
        let bar_max = width.saturating_sub(14).clamp(10, 40);
        // Floor instead of round so the bar never visually tops out.
        let filled = ((bar_max as f32) * progress).floor() as usize;
        let filled = filled.min(bar_max.saturating_sub(1));
        let bar = format!(
            "{}{}",
            "█".repeat(filled),
            "░".repeat(bar_max.saturating_sub(filled))
        );

        Line::from(vec![
            Span::styled("    ", dim),
            Span::styled(FRAMES[frame_idx].to_string(), Style::default().fg(theme.accent)),
            Span::raw(" "),
            Span::styled(bar, Style::default().fg(theme.accent)),
            Span::styled(
                format!(" {:.0}%", progress * 100.0),
                dim,
            ),
        ])
    }

    fn title_text(&self) -> &str {
        match self.status {
            ToolStatus::Running => "Running",
            ToolStatus::Success => "Ran",
            ToolStatus::Failed => "Failed",
        }
    }
}

impl ChatCell for ToolChatCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn as_any_ref(&self) -> &dyn std::any::Any {
        self
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let dim = Style::default().dim();
        let w = width as usize;

        let header = if self.status == ToolStatus::Running {
            let text = format!(
                "{} {} ({})",
                self.title_text(),
                self.name,
                self.elapsed_str()
            );
            let mut spans = vec![self.bullet()];
            spans.extend(crate::tui::shimmer::shimmer_spans(&text));
            Line::from(spans)
        } else {
            Line::from(vec![
                self.bullet(),
                Span::styled(format!("{} ", self.title_text()), Style::default().bold()),
                Span::raw(self.name.clone()),
                Span::styled(format!(" ({})", self.elapsed_str()), dim),
            ])
        };

        let mut lines = vec![header];

        // For running tools that have been going for a while, add a
        // Braille spinner line so the user can see the turn hasn't
        // frozen. Shown only after 3s to avoid visual noise on fast
        // tool calls.
        if self.status == ToolStatus::Running {
            let elapsed = self.started_at.elapsed().as_millis() as u64;
            if elapsed >= 3_000 {
                lines.push(self.progress_line(w, elapsed));
            }
        }

        // Command/description with │ prefix
        if !self.description.is_empty() {
            let max_w = w.saturating_sub(4);
            for dl in self.description.lines().take(2) {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", dim),
                    Span::raw(truncate_by_width(dl, max_w)),
                ]));
            }
        }

        // Output summary with └ prefix, using diff renderer for +/- lines
        if let Some(ref summary) = self.output_summary {
            let has_diff = summary
                .lines()
                .any(|l| l.starts_with('+') || l.starts_with('-'));
            if has_diff {
                let diff_lines = crate::tui::diff_render::render_diff_lines(summary, 8);
                for (i, dl) in diff_lines.into_iter().enumerate() {
                    if i == 0 {
                        let mut spans = vec![Span::styled("  └ ", dim)];
                        spans.extend(dl.spans);
                        lines.push(Line::from(spans));
                    } else {
                        lines.push(dl);
                    }
                }
            } else {
                let max_w = w.saturating_sub(4);
                let out_lines: Vec<&str> = summary.lines().take(5).collect();
                for (i, ol) in out_lines.iter().enumerate() {
                    let prefix = if i == 0 {
                        Span::styled("  └ ", dim)
                    } else {
                        Span::raw("    ")
                    };
                    lines.push(Line::from(vec![
                        prefix,
                        Span::raw(truncate_by_width(ol, max_w)),
                    ]));
                }
                if summary.lines().count() > 5 {
                    let remaining = summary.lines().count() - 5;
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(format!("… +{remaining} lines"), dim),
                    ]));
                }
            }
        }

        lines
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.display_lines(width);
        // Full output in transcript using diff renderer
        if let Some(ref output) = self.output {
            if !output.trim().is_empty() {
                lines.push(Line::default());
                let diff_lines = crate::tui::diff_render::render_diff_lines(output, 100);
                lines.extend(diff_lines);
            }
        }
        lines
    }
}

fn truncate_by_width(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    let mut width = 0;
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw + 1 > max_width {
            break;
        }
        width += cw;
        end = i + c.len_utf8();
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn text_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn short_running_shows_no_progress_line() {
        let cell = ToolChatCell::new_running("bash".into(), "ls".into());
        // 0 elapsed → no progress line.
        let lines = cell.display_lines(80);
        assert_eq!(lines.len(), 2, "header + description only");
    }

    #[test]
    fn progress_line_contains_spinner_and_bar() {
        let cell = ToolChatCell::new_running("bash".into(), String::new());
        let line = cell.progress_line(80, 5_000);
        let t = text_of(&line);
        assert!(
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                .iter()
                .any(|g| t.contains(g)),
            "spinner glyph missing; got {t:?}"
        );
        assert!(t.contains("█") || t.contains("░"), "bar missing; got {t:?}");
        assert!(t.contains("%"), "percent label missing; got {t:?}");
    }

    #[test]
    fn progress_fill_monotonically_grows() {
        let cell = ToolChatCell::new_running("bash".into(), String::new());
        let early = cell.progress_line(80, 3_000);
        let late = cell.progress_line(80, 25_000);
        let early_fill = text_of(&early).matches('█').count();
        let late_fill = text_of(&late).matches('█').count();
        assert!(
            late_fill > early_fill,
            "bar should grow over time; early={early_fill} late={late_fill}"
        );
    }

    #[test]
    fn progress_percent_caps_below_100() {
        // Even after an hour we shouldn't show >=100%.
        let cell = ToolChatCell::new_running("bash".into(), String::new());
        let line = cell.progress_line(80, 3_600_000);
        let t = text_of(&line);
        assert!(!t.contains(" 100%"), "should asymptote below 100%; got {t}");
    }
}
