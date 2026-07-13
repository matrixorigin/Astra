use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::{
    cli::slash::slash_inspect::{InspectorFactStatus, WorkbenchInspection},
    tui::theme,
};

const MAX_VISIBLE: usize = 14;

/// A read-only scrollable text view for displaying command output inline.
pub(crate) struct InfoView {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: usize,
    completed: bool,
    reopen: Option<String>,
    primary_workspace: bool,
    primary_visible_rows: Cell<usize>,
}

impl InfoView {
    pub fn new(title: String, lines: Vec<Line<'static>>) -> Self {
        Self {
            title,
            lines,
            scroll: 0,
            completed: false,
            reopen: None,
            primary_workspace: false,
            primary_visible_rows: Cell::new(MAX_VISIBLE),
        }
    }

    pub fn with_reopen(mut self, parent: &str) -> Self {
        self.reopen = Some(parent.to_string());
        self
    }

    /// Promote a dense, user-requested evidence report into the workbench's
    /// primary canvas. Short confirmations and pickers remain bounded
    /// overlays; this is for reports that need room to be read and compared.
    pub fn with_primary_workspace(mut self) -> Self {
        self.primary_workspace = true;
        self
    }

    pub fn from_plain(title: &str, text: Vec<String>) -> Self {
        // Plain command output is still primary content. Rendering the entire
        // panel as dim makes facts, diagnostics, and action guidance read as
        // inactive chrome; callers that need a secondary line can provide a
        // structured view instead.
        let content = Style::default().fg(theme::current().fg);
        let lines: Vec<Line<'static>> = text
            .into_iter()
            .map(|s| Line::from(Span::styled(s, content)))
            .collect();
        Self::new(title.to_string(), lines)
    }

    pub fn from_key_value(title: &str, pairs: Vec<(&str, String)>) -> Self {
        let theme = theme::current();
        let dim = Style::default().fg(theme.dim);
        let val_style = Style::default().fg(theme.fg);
        let lines: Vec<Line<'static>> = pairs
            .into_iter()
            .map(|(key, val)| {
                Line::from(vec![
                    Span::styled(format!("  {:<16}", format!("{key}:")), dim),
                    Span::styled(val, val_style),
                ])
            })
            .collect();
        Self::new(title.to_string(), lines)
    }

    /// Present an inspection payload without treating missing evidence as a
    /// successful value.  Keeping the status until this final render step
    /// makes the same facts safe to reuse in non-TUI surfaces.
    pub fn from_inspection(title: &str, inspection: WorkbenchInspection) -> Self {
        let theme = theme::current();
        let dim = Style::default().fg(theme.dim);
        let heading = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let observed = Style::default().fg(theme.fg);
        let unavailable = Style::default().fg(theme.dim);
        let degraded = Style::default().fg(theme.error);
        let mut lines = Vec::new();

        for (section_index, section) in inspection.sections.into_iter().enumerate() {
            if section_index > 0 {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(section.title, heading)));
            lines.push(Line::from(vec![
                Span::styled("  Source · ", dim),
                Span::styled(section.source, dim),
            ]));
            for fact in section.facts {
                let value_style = match fact.status {
                    InspectorFactStatus::Observed => observed,
                    InspectorFactStatus::NotRecorded => unavailable,
                    InspectorFactStatus::Degraded => degraded,
                };
                let marker = match fact.status {
                    InspectorFactStatus::Observed => "  ",
                    InspectorFactStatus::NotRecorded => "  · ",
                    InspectorFactStatus::Degraded => "  ! ",
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, dim),
                    Span::styled(format!("{:<22}", fact.label), dim),
                    Span::styled(fact.value, value_style),
                ]));
            }
        }

        Self::new(title.to_string(), lines)
    }

    /// Render a typed reflection without flattening its observations,
    /// evidence, and advisory proposals into one indistinguishable log.  A
    /// reflection is read-only evidence: this view intentionally exposes no
    /// implicit apply action.
    pub fn from_reflection(
        title: &str,
        provenance: &str,
        report: astra_services::reflect::ReflectReport,
    ) -> Self {
        let theme = theme::current();
        let dim = Style::default().fg(theme.dim);
        let heading = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let normal = Style::default().fg(theme.fg);
        let warn = Style::default().fg(theme.warn);
        let error = Style::default().fg(theme.error);
        let advisory = Style::default().fg(theme.accent);
        let mut lines = vec![
            Line::from(Span::styled("Provenance", heading)),
            Line::from(vec![
                Span::styled("  Source · ", dim),
                Span::styled(provenance.to_string(), normal),
            ]),
            Line::from(vec![
                Span::styled("  Coverage · ", dim),
                Span::styled(
                    format!(
                        "{} · {} · {} events · {} decisions",
                        report.data_coverage.overall,
                        report.data_coverage.source,
                        report.data_coverage.events,
                        report.data_coverage.decisions,
                    ),
                    normal,
                ),
            ]),
            Line::from(vec![
                Span::styled("  Scope · ", dim),
                Span::styled(
                    format!(
                        "{} / {} · {} · {}",
                        report.topic, report.facet, report.horizon, report.depth
                    ),
                    dim,
                ),
            ]),
        ];

        if !report.summary.trim().is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("Summary", heading)));
            lines.push(Line::from(Span::styled(
                format!("  {}", report.summary),
                normal,
            )));
        }

        for coverage_warning in &report.data_coverage.warnings {
            lines.push(Line::from(vec![
                Span::styled("  Coverage warning · ", warn),
                Span::styled(coverage_warning.clone(), warn),
            ]));
        }

        if !report.observations.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("Observed findings", heading)));
            for observation in &report.observations {
                let style = match observation.severity.as_str() {
                    "critical" | "error" | "failed" => error,
                    "warning" | "warn" => warn,
                    _ => normal,
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  [{}] ", observation.severity), style),
                    Span::styled(observation.summary.clone(), style),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("    observation · ", dim),
                    Span::styled(observation.ref_id.clone(), dim),
                ]));
                if !observation.evidence_refs.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("    evidence refs · ", dim),
                        Span::styled(compact_refs(&observation.evidence_refs), dim),
                    ]));
                }
            }
        }

        if !report.failure_clusters.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("Failure clusters", heading)));
            for cluster in &report.failure_clusters {
                lines.push(Line::from(Span::styled(
                    format!("  {} · {}", cluster.label, cluster.summary),
                    warn,
                )));
                lines.push(Line::from(vec![
                    Span::styled("    cluster · ", dim),
                    Span::styled(cluster.cluster_ref.clone(), dim),
                ]));
            }
        }

        if !report.evidence.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("Evidence", heading)));
            for evidence in &report.evidence {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} · ", evidence.evidence_class), dim),
                    Span::styled(evidence.summary.clone(), normal),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("    source · ", dim),
                    Span::styled(evidence.source.clone(), dim),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("    ref · ", dim),
                    Span::styled(evidence.ref_id.clone(), dim),
                ]));
            }
        }

        if !report.action_hints.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("Advisory next steps", heading)));
            for hint in &report.action_hints {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} · ", hint.target_type), advisory),
                    Span::styled(hint.summary.clone(), advisory),
                ]));
                if !hint.observation_refs.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("    supported by · ", dim),
                        Span::styled(compact_refs(&hint.observation_refs), dim),
                    ]));
                }
            }
        }

        if report.budget_result.truncated {
            let omitted = &report.budget_result.omitted;
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!(
                    "Evidence bounded · omitted {} observations, {} previews, {} hints",
                    omitted.observations, omitted.evidence_previews, omitted.action_hints
                ),
                warn,
            )));
            if let Some(cursor) = report.budget_result.next_cursor.as_deref() {
                lines.push(Line::from(Span::styled(
                    format!("  Continuation cursor · {cursor}"),
                    dim,
                )));
            }
        }

        if report.observations.is_empty()
            && report.evidence.is_empty()
            && report.action_hints.is_empty()
            && report.failure_clusters.is_empty()
        {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "No observation records were returned for this scope.",
                dim,
            )));
        }

        Self::new(title.to_string(), lines)
    }

    fn max_visible_rows(&self) -> usize {
        if self.primary_workspace {
            self.primary_visible_rows.get().max(1)
        } else {
            MAX_VISIBLE
        }
    }

    fn visible_count(&self) -> usize {
        self.lines.len().min(self.max_visible_rows())
    }
}

fn compact_refs(refs: &[String]) -> String {
    let mut values: Vec<&str> = refs.iter().take(2).map(String::as_str).collect();
    if refs.len() > values.len() {
        values.push("…");
    }
    values.join(" · ")
}

impl BottomPaneView for InfoView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 3 {
            return;
        }

        let theme = theme::current();
        let dim = Style::default().fg(theme.dim);
        let title_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let mut y = area.y;
        if self.primary_workspace {
            // Reserve title, spacer, scroll indicator, spacer, and hint.
            // The compact overlay keeps its fixed 14-row contract.
            self.primary_visible_rows
                .set(usize::from(area.height.saturating_sub(5)).max(1));
        }
        let max_visible = self.max_visible_rows();

        // Title
        if y < area.bottom() {
            let line = Line::from(Span::styled(format!("  {}", self.title), title_style));
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Blank
        if y < area.bottom() {
            y += 1;
        }

        // Content
        let visible_end = (self.scroll + self.visible_count()).min(self.lines.len());
        for i in self.scroll..visible_end {
            if y >= area.bottom() {
                break;
            }
            Widget::render(
                self.lines[i].clone(),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // Scroll indicator if needed
        if self.lines.len() > max_visible && y < area.bottom() {
            let pos = self.scroll + 1;
            let total = self.lines.len();
            let indicator = Line::from(Span::styled(
                format!("  ({pos}–{visible_end} of {total})"),
                dim,
            ));
            Widget::render(indicator, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Hint
        if y < area.bottom() {
            y += 1;
        }
        if y < area.bottom() {
            let hint = if self.lines.len() > max_visible {
                "  ↑/↓ scroll  Esc close"
            } else {
                "  Esc close"
            };
            Widget::render(
                Line::from(Span::styled(hint, dim)),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let title_h = 2; // title + blank
        let content_h = self.visible_count() as u16;
        let scroll_h = if self.lines.len() > MAX_VISIBLE { 1 } else { 0 };
        let hint_h = 2; // blank + hint
        title_h + content_h + scroll_h + hint_h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let max_visible = self.max_visible_rows();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.scroll + max_visible < self.lines.len() => {
                self.scroll += 1;
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(max_visible);
            }
            KeyCode::PageDown => {
                self.scroll =
                    (self.scroll + max_visible).min(self.lines.len().saturating_sub(max_visible));
            }
            KeyCode::Esc | KeyCode::Enter => {
                self.completed = true;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.completed {
            Some(ViewCompletion {
                result: None,
                reopen: self.reopen.clone(),
            })
        } else {
            None
        }
    }

    fn owns_primary_canvas(&self) -> bool {
        self.primary_workspace
    }
}

#[cfg(test)]
mod tests {
    use super::InfoView;
    use crate::{
        cli::slash::slash_inspect::{
            InspectorFact, InspectorFactStatus, InspectorSection, WorkbenchInspection,
        },
        tui::bottom_pane::view::BottomPaneView,
        tui::theme,
    };

    #[test]
    fn plain_info_content_uses_primary_reading_contrast() {
        let view = InfoView::from_plain("Info", vec!["actual content".into()]);

        assert_eq!(view.lines[0].spans[0].style.fg, Some(theme::current().fg));
    }

    #[test]
    fn evidence_reports_can_explicitly_own_the_primary_workspace() {
        let view = InfoView::from_plain("Evidence", vec!["fact".into()]).with_primary_workspace();

        assert!(view.owns_primary_canvas());
    }

    #[test]
    fn inspection_view_keeps_missing_and_degraded_evidence_distinct() {
        let view = InfoView::from_inspection(
            "Inspector",
            WorkbenchInspection {
                sections: vec![InspectorSection {
                    title: "State".into(),
                    source: "test evidence".into(),
                    facts: vec![
                        InspectorFact {
                            label: "Trace".into(),
                            value: "not recorded".into(),
                            status: InspectorFactStatus::NotRecorded,
                        },
                        InspectorFact {
                            label: "Persistence".into(),
                            value: "write failed".into(),
                            status: InspectorFactStatus::Degraded,
                        },
                    ],
                }],
            },
        );

        assert_eq!(view.lines[2].spans[2].style.fg, Some(theme::current().dim));
        assert_eq!(
            view.lines[3].spans[2].style.fg,
            Some(theme::current().error)
        );
    }

    #[test]
    fn reflection_view_keeps_findings_and_advisories_semantically_distinct() {
        let report = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "tool": "reflect",
            "session_id": "session-test",
            "analysis_view": "overview",
            "topic": "execution",
            "facet": "errors",
            "depth": "diagnostic",
            "horizon": "session",
            "source_policy": "auto",
            "include_context": false,
            "data_coverage": {
                "overall": "fresh",
                "source": "session_journal",
                "events": 1,
                "decisions": 0
            },
            "observations": [{
                "ref_id": "urn:astra:observation:local:reflect:session:0",
                "topic": "execution",
                "facet": "errors",
                "kind": "tool_error",
                "severity": "warning",
                "summary": "The command failed",
                "confidence": {"evidence": 0.9}
            }],
            "action_hints": [{
                "target_type": "user_guidance",
                "summary": "Narrow the scope",
                "confidence": {"evidence": 0.9}
            }]
        }))
        .expect("valid current reflection payload");

        let view = InfoView::from_reflection("Reflection", "local artifacts", report);

        assert_eq!(view.lines[6].spans[0].style.fg, Some(theme::current().warn));
        assert_eq!(
            view.lines[10].spans[0].style.fg,
            Some(theme::current().accent)
        );
    }
}
