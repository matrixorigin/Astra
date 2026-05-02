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
        }
    }

    pub fn complete(&mut self, status_str: &str, duration_ms: u64, description: String, output_summary: Option<String>) {
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
    }

    fn bullet(&self) -> Span<'static> {
        match self.status {
            ToolStatus::Running => Span::styled("• ", Style::default().dim()),
            ToolStatus::Success => Span::styled("• ", Style::default().fg(Color::Green).bold()),
            ToolStatus::Failed => Span::styled("• ", Style::default().fg(Color::Red).bold()),
        }
    }

    fn elapsed_str(&self) -> String {
        if let Some(ms) = self.duration_ms {
            if ms < 1000 { format!("{ms}ms") } else { format!("{:.1}s", ms as f64 / 1000.0) }
        } else {
            let ms = self.started_at.elapsed().as_millis();
            if ms < 1000 { format!("{ms}ms") } else { format!("{:.1}s", ms as f64 / 1000.0) }
        }
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
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
    fn as_any_ref(&self) -> &dyn std::any::Any { self }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let dim = Style::default().dim();
        let w = width as usize;

        let header = if self.status == ToolStatus::Running {
            let text = format!("{} {} ({})", self.title_text(), self.name, self.elapsed_str());
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

        // Output with └ prefix (first line) then 4-space indent
        // Diff lines (+/-) get green/red coloring
        if let Some(ref summary) = self.output_summary {
            let max_w = w.saturating_sub(8);
            let out_lines: Vec<&str> = summary.lines().take(8).collect();
            for (i, ol) in out_lines.iter().enumerate() {
                let prefix = if i == 0 {
                    Span::styled("  └ ", dim)
                } else {
                    Span::raw("    ")
                };
                let content = truncate_by_width(ol, max_w);
                let style = if ol.starts_with('+') {
                    Style::default().fg(Color::Green)
                } else if ol.starts_with('-') {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![prefix, Span::styled(content, style)]));
            }
            if summary.lines().count() > 8 {
                let remaining = summary.lines().count() - 8;
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("… +{remaining} lines"), dim),
                ]));
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
        if width + cw + 1 > max_width { break; }
        width += cw;
        end = i + c.len_utf8();
    }
    format!("{}…", &s[..end])
}
