//! Task-invocation history cell — the `▶ Task · <description>` block
//! that mirrors the `Task` tool UX from claude-code.
//!
//! Unlike [`ToolCell`](super::tool::ToolCell), a TaskCell is a
//! **container**: it owns the header for the parent tool invocation
//! plus a vector of child events that were emitted inside the task's
//! execution (each child tool call, for example). Children are
//! routed in by `ChatWidget` when a wire event carries
//! `parent_tool_use_id = <this cell's tool_use_id>`.
//!
//! Three visual states:
//! - **Running** — accent arrow, shimmer title, children render
//!   live underneath.
//! - **Completed** — green arrow, `Task · <name> · done · 42ms`
//!   title, output
//!   summary folded under the children.
//! - **Failed** — red arrow, same layout plus an error line.
//!
//! Persists as a compact summary (count of children, final status,
//! duration) via `TurnEvent::Task` — full child detail is already
//! persisted through each child's own tool record.

use std::any::Any;
use std::time::Instant;

use super::truncate_by_width;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::HistoryCell;
use crate::cli::tool_result_status::tool_result_status_is_success;
use crate::tui::agent_control_status::AGENT_RESULT_INTERRUPTED_ERROR;
use crate::tui::history_cell::tool::humanize_tool_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Running,
    Completed,
    Failed,
}

/// A single child event rendered inside the task body. Minimal
/// shape: we only surface the header line for each inner tool call,
/// not the full ToolCell bells and whistles — the point is an
/// at-a-glance progress list, not another scrollback.
#[derive(Debug, Clone)]
pub(crate) struct TaskChildEvent {
    pub tool_use_id: String,
    pub name: String,
    pub description: String,
    pub status: ChildStatus,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildStatus {
    Running,
    Success,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskCell {
    pub tool_use_id: String,
    pub description: String,
    pub status: TaskStatus,
    pub started_at: Instant,
    pub completed_at: Option<Instant>,
    pub duration_ms: Option<u64>,
    pub output_summary: Option<String>,
    pub children: Vec<TaskChildEvent>,
    pub error: Option<String>,
    pub ctrl_b_background_hint: bool,
}

impl TaskCell {
    pub fn new_running(tool_use_id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            description: description.into(),
            status: TaskStatus::Running,
            started_at: Instant::now(),
            completed_at: None,
            duration_ms: None,
            output_summary: None,
            children: Vec::new(),
            error: None,
            ctrl_b_background_hint: false,
        }
    }

    pub fn set_ctrl_b_background_hint(&mut self, enabled: bool) {
        self.ctrl_b_background_hint = enabled;
    }

    /// Record a child tool starting inside this task. Idempotent by
    /// `tool_use_id`: if we've seen this child already (e.g. replay)
    /// the existing row stays and nothing is duplicated.
    pub fn push_child_started(
        &mut self,
        tool_use_id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) {
        let id = tool_use_id.into();
        if self.children.iter().any(|c| c.tool_use_id == id) {
            return;
        }
        self.children.push(TaskChildEvent {
            tool_use_id: id,
            name: name.into(),
            description: description.into(),
            status: ChildStatus::Running,
            duration_ms: None,
        });
    }

    /// Flip a previously-started child to its terminal status. No-op
    /// if the child isn't known (out-of-order replay).
    pub fn push_child_completed(&mut self, tool_use_id: &str, status_str: &str, duration_ms: u64) {
        if let Some(c) = self
            .children
            .iter_mut()
            .find(|c| c.tool_use_id == tool_use_id)
        {
            c.status = if tool_result_status_is_success(status_str) {
                ChildStatus::Success
            } else {
                ChildStatus::Failed
            };
            c.duration_ms = Some(duration_ms);
        }
    }

    /// Terminal transition for the parent task. `status_str` follows
    /// the shared canonical tool-result convention:
    /// `"completed"` = green, anything else except `"skipped"` = failed.
    pub fn complete(
        &mut self,
        status_str: &str,
        duration_ms: u64,
        output_summary: Option<String>,
        error: Option<String>,
    ) {
        self.status = if tool_result_status_is_success(status_str) {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        };
        self.completed_at = Some(Instant::now());
        self.duration_ms = Some(duration_ms);
        self.output_summary = output_summary;
        self.error = error;
    }

    fn is_agent_result_wait(&self) -> bool {
        self.description.starts_with("Get agent result:")
    }

    pub(crate) fn is_interrupted_wait(&self) -> bool {
        self.status == TaskStatus::Failed
            && self.is_agent_result_wait()
            && self.error.as_deref() == Some(AGENT_RESULT_INTERRUPTED_ERROR)
    }

    fn arrow(&self) -> Span<'static> {
        match self.status {
            TaskStatus::Running => {
                let theme = crate::tui::theme::current();
                Span::styled("▶ ", Style::default().fg(theme.accent).bold())
            }
            TaskStatus::Completed => Span::styled("▶ ", Style::default().fg(Color::Green).bold()),
            TaskStatus::Failed if self.is_interrupted_wait() => {
                Span::styled("▶ ", Style::default().fg(Color::Yellow).bold())
            }
            TaskStatus::Failed => Span::styled("▶ ", Style::default().fg(Color::Red).bold()),
        }
    }

    fn title_text(&self) -> &'static str {
        match self.status {
            TaskStatus::Running => "running",
            TaskStatus::Completed => "done",
            TaskStatus::Failed if self.is_interrupted_wait() => "interrupted",
            TaskStatus::Failed => "failed",
        }
    }

    fn elapsed_str(&self) -> String {
        let ms = self
            .duration_ms
            .unwrap_or_else(|| self.started_at.elapsed().as_millis() as u64);
        if ms < 1000 {
            format!("{ms}ms")
        } else {
            format!("{:.1}s", ms as f64 / 1000.0)
        }
    }
}

fn child_display_name(name: &str) -> String {
    match name {
        "bash" => "Bash".into(),
        "read" | "read_file" => "Read".into(),
        "write_file" => "Write file".into(),
        "str_replace" => "Replace text".into(),
        "grep" | "glob" => "Search".into(),
        "list_dir" => "List directory".into(),
        "task" => "Task".into(),
        "memory" => "Memory".into(),
        "tool_search" => "Tool search".into(),
        _ => humanize_tool_name(name),
    }
}

/// Max children shown inline when the task is no longer running.
/// Beyond this threshold the cell collapses to a summary line.
const COLLAPSE_THRESHOLD: usize = 3;

impl HistoryCell for TaskCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let dim = Style::default().dim();
        let w = width as usize;
        let max_child_w = w.saturating_sub(6);

        // Header.
        let task_label = "Task";
        let background_hint = if self.status == TaskStatus::Running && self.ctrl_b_background_hint {
            " · Ctrl+B to background"
        } else {
            ""
        };
        let header = if self.status == TaskStatus::Running {
            let meta = format!(" · {}{}", self.elapsed_str(), background_hint);
            let desc = trimmed_desc(
                &self.description,
                w.saturating_sub(2 + task_label.width() + 3 + meta.width()),
            );
            Line::from(vec![
                self.arrow(),
                Span::styled(task_label, Style::default().bold()),
                Span::styled(" · ", dim),
                Span::raw(desc),
                Span::styled(meta, dim),
            ])
        } else {
            let meta = format!("{} · {}", self.title_text(), self.elapsed_str());
            let meta_text = format!(" · {meta}");
            let desc = trimmed_desc(
                &self.description,
                w.saturating_sub(2 + task_label.width() + 3 + meta_text.width()),
            );
            Line::from(vec![
                self.arrow(),
                Span::styled(task_label, Style::default().bold()),
                Span::styled(" · ", dim),
                Span::raw(desc),
                Span::styled(meta_text, dim),
            ])
        };

        let mut lines = vec![header];

        // Children rendering: inline while running or ≤ threshold,
        // collapsed summary when completed with many children.
        let should_collapse =
            self.status != TaskStatus::Running && self.children.len() > COLLAPSE_THRESHOLD;

        if should_collapse {
            // Collapsed summary: "  └ 12 steps · 10 done, 2 failed"
            let succeeded = self
                .children
                .iter()
                .filter(|c| c.status == ChildStatus::Success)
                .count();
            let failed = self
                .children
                .iter()
                .filter(|c| c.status == ChildStatus::Failed)
                .count();
            let total = self.children.len();
            let summary = if failed > 0 {
                format!("{total} steps · {succeeded} done, {failed} failed")
            } else {
                format!("{total} steps · all done")
            };
            lines.push(Line::from(vec![
                Span::styled("  └ ", dim),
                Span::styled(summary, dim),
            ]));
        } else {
            // Inline children — calmer "action + object" phrasing:
            // `  ├ Read src/main.rs · 20ms`.
            let total = self.children.len();
            for (i, child) in self.children.iter().enumerate() {
                let is_last = i + 1 == total && self.status != TaskStatus::Running;
                let connector = if is_last { "  └ " } else { "  ├ " };
                let name_style = match child.status {
                    ChildStatus::Running => Style::default().fg(Color::Gray).bold(),
                    ChildStatus::Success => Style::default().fg(Color::Green).bold(),
                    ChildStatus::Failed => Style::default().fg(Color::Red).bold(),
                };
                let meta = match child.status {
                    ChildStatus::Running => None,
                    ChildStatus::Success => child.duration_ms.map(format_duration_ms),
                    ChildStatus::Failed => child
                        .duration_ms
                        .map(|ms| format!("failed · {}", format_duration_ms(ms)))
                        .or_else(|| Some("failed".into())),
                };
                let name = child_display_name(&child.name);
                let desc = truncate_by_width(
                    &child.description,
                    max_child_w.saturating_sub(
                        name.width()
                            + meta.as_deref().map(UnicodeWidthStr::width).unwrap_or(0)
                            + if child.description.is_empty() { 0 } else { 1 }
                            + if meta.is_some() { 3 } else { 0 },
                    ),
                );
                let theme = crate::tui::theme::current();
                let mut spans = vec![
                    Span::styled(connector.to_string(), dim),
                    Span::styled(name, name_style),
                ];
                if !desc.is_empty() {
                    spans.push(Span::raw(" "));
                    // Apply semantic styling based on tool type.
                    if child.name == "bash" {
                        if let Some(cmd) = desc.strip_prefix("$ ") {
                            spans.push(Span::styled("$ ", dim));
                            spans.push(Span::styled(cmd.to_string(), theme.command_style()));
                        } else {
                            spans.push(Span::styled(desc.to_string(), theme.command_style()));
                        }
                    } else if matches!(
                        child.name.as_str(),
                        "read" | "read_file" | "write_file" | "grep" | "glob" | "list_dir"
                    ) && desc.contains('/')
                    {
                        spans.extend(crate::tui::path_style::style_file_path_flat(
                            &desc,
                            Style::default(),
                        ));
                    } else {
                        spans.push(Span::raw(desc.to_string()));
                    }
                }
                if let Some(meta) = meta {
                    spans.push(Span::styled(format!(" · {meta}"), dim));
                }
                lines.push(Line::from(spans));
            }
        }

        // Output summary (post-complete only, collapsed = just line count).
        // Keep it in the same tree language as the child rows rather than
        // switching to a different arrow glyph mid-cell.
        if let Some(ref summary) = self.output_summary
            && self.status != TaskStatus::Running
        {
            if should_collapse {
                let lc = summary.lines().count();
                if lc > 0 {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(format!("{lc} lines of output"), dim),
                    ]));
                }
            } else {
                let max_w = w.saturating_sub(4);
                for (i, sl) in summary.lines().take(4).enumerate() {
                    lines.push(Line::from(vec![
                        if i == 0 {
                            Span::styled("  └ ", dim)
                        } else {
                            Span::raw("    ")
                        },
                        Span::raw(truncate_by_width(sl, max_w)),
                    ]));
                }
                let lc = summary.lines().count();
                if lc > 4 {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(format!("… +{} lines", lc - 4), dim),
                    ]));
                }
            }
        }

        if let Some(ref err) = self.error {
            let (icon_style, text_style) = if self.is_interrupted_wait() {
                (
                    Style::default().fg(Color::Yellow).bold(),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                (
                    Style::default().fg(Color::Red).bold(),
                    Style::default().fg(Color::Red),
                )
            };
            lines.push(Line::from(vec![
                Span::styled("  · Error ", icon_style),
                Span::styled(err.clone(), text_style),
            ]));
        }

        lines
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn is_live(&self) -> bool {
        self.status == TaskStatus::Running
    }

    fn finalize(&mut self) {
        if self.status == TaskStatus::Running {
            self.status = TaskStatus::Failed;
            self.completed_at = Some(Instant::now());
            if self.duration_ms.is_none() {
                self.duration_ms = Some(self.started_at.elapsed().as_millis() as u64);
            }
            if self.error.is_none() {
                self.error = Some(if self.is_agent_result_wait() {
                    AGENT_RESULT_INTERRUPTED_ERROR.into()
                } else {
                    "Task did not complete before the turn ended.".into()
                });
            }
        }
    }
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn trimmed_desc(desc: &str, max_w: usize) -> String {
    if desc.is_empty() {
        return String::from("(no description)");
    }
    truncate_by_width(desc, max_w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &TaskCell, width: u16, height: u16) -> String {
        let lines = cell.display_lines(width);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, height))
    }

    // ── Lifecycle ──────────────────────────────────────────────────

    #[test]
    fn running_task_is_live() {
        let t = TaskCell::new_running("tu_parent", "investigate flaky test");
        assert!(t.is_live());
        assert_eq!(t.status, TaskStatus::Running);
    }

    #[test]
    fn complete_transitions_out_of_running() {
        let mut t = TaskCell::new_running("tu_parent", "do work");
        t.complete("completed", 1234, Some("done".into()), None);
        assert!(!t.is_live());
        assert_eq!(t.status, TaskStatus::Completed);
        assert_eq!(t.duration_ms, Some(1234));
    }

    #[test]
    fn failure_sets_status_failed_and_preserves_error() {
        let mut t = TaskCell::new_running("tu_parent", "risky op");
        t.complete("failed", 10, None, Some("boom".into()));
        assert_eq!(t.status, TaskStatus::Failed);
        assert_eq!(t.error.as_deref(), Some("boom"));
    }

    #[test]
    fn finalize_demotes_stuck_running_to_failed_with_placeholder_error() {
        let mut t = TaskCell::new_running("tu_parent", "slow");
        t.finalize();
        assert_eq!(t.status, TaskStatus::Failed);
        assert!(t.duration_ms.is_some());
        assert!(
            t.completed_at.is_some(),
            "finalize is a terminal transition and must stamp local completion time"
        );
        assert!(t.error.is_some(), "finalize must stamp a reason");
    }

    #[test]
    fn finalize_get_agent_result_renders_as_interrupted_not_failed() {
        let mut t = TaskCell::new_running("tu_parent", "Get agent result: reviewer@abc");
        t.push_child_started("child-1", "bash", "git diff");
        t.push_child_completed("child-1", "completed", 20);

        t.finalize();

        let out = render(&t, 100, 4);
        let normalized = out.replace('\n', " ");
        assert!(
            out.contains("Task · Get agent result: reviewer@abc · interrupted"),
            "{out}"
        );
        assert!(
            !out.contains("Task · Get agent result: reviewer@abc · failed"),
            "{out}"
        );
        assert!(normalized.contains(AGENT_RESULT_INTERRUPTED_ERROR), "{out}");
    }

    // ── Children ───────────────────────────────────────────────────

    #[test]
    fn push_child_started_is_idempotent_on_duplicate_id() {
        let mut t = TaskCell::new_running("tu_parent", "wrap");
        t.push_child_started("tu_a", "bash", "ls");
        t.push_child_started("tu_a", "bash", "ls");
        assert_eq!(t.children.len(), 1);
    }

    #[test]
    fn push_child_completed_flips_status_and_duration() {
        let mut t = TaskCell::new_running("tu_parent", "wrap");
        t.push_child_started("tu_a", "bash", "ls");
        t.push_child_completed("tu_a", "completed", 55);
        assert_eq!(t.children[0].status, ChildStatus::Success);
        assert_eq!(t.children[0].duration_ms, Some(55));
    }

    #[test]
    fn push_child_completed_accepts_success_aliases_from_tool_output() {
        let mut t = TaskCell::new_running("tu_parent", "wrap");
        t.push_child_started("tu_a", "bash", "ls");
        t.push_child_completed("tu_a", "ok", 55);
        assert_eq!(t.children[0].status, ChildStatus::Success);
    }

    #[test]
    fn push_child_completed_is_noop_for_unknown_id() {
        let mut t = TaskCell::new_running("tu_parent", "wrap");
        t.push_child_completed("tu_missing", "completed", 1);
        assert!(
            t.children.is_empty(),
            "no child should appear from a stray completed event"
        );
    }

    // ── Render golden paths ────────────────────────────────────────

    #[test]
    fn running_header_has_task_label_without_shimmer_noise() {
        let t = TaskCell::new_running("tu_parent", "audit cache correctness");
        let out = render(&t, 80, 2);
        assert!(out.contains("▶"), "missing arrow: {out}");
        assert!(out.contains("Task ·"), "missing label: {out}");
        assert!(
            out.contains("audit cache correctness"),
            "missing desc: {out}"
        );
        assert!(
            !out.contains("✶") && !out.contains("✷") && !out.contains("✹") && !out.contains("✺"),
            "running task header should be calmer than the old shimmer treatment: {out}"
        );
    }

    #[test]
    fn running_header_can_advertise_ctrl_b_backgrounding() {
        let mut t = TaskCell::new_running(
            "tu_parent",
            "Spawn agent: reviewer with a deliberately long role label",
        );
        t.set_ctrl_b_background_hint(true);
        let out = render(&t, 48, 2);

        assert!(out.contains("Ctrl+B to background"), "{out}");
    }

    #[test]
    fn completed_header_shows_task_done_and_duration() {
        let mut t = TaskCell::new_running("tu_parent", "do work");
        t.complete("completed", 2500, Some("3 files changed".into()), None);
        let out = render(&t, 80, 4);
        assert!(
            out.contains("Task · do work · done"),
            "missing completed label: {out}"
        );
        assert!(out.contains("2.5s"), "missing duration: {out}");
        assert!(out.contains("3 files changed"), "missing summary: {out}");
    }

    #[test]
    fn complete_accepts_success_aliases_from_tool_output() {
        let mut t = TaskCell::new_running("tu_parent", "do work");
        t.complete("ok", 2500, Some("3 files changed".into()), None);
        assert_eq!(t.status, TaskStatus::Completed);
    }

    #[test]
    fn failed_header_shows_task_failed_and_error_line() {
        let mut t = TaskCell::new_running("tu_parent", "risky");
        t.complete("failed", 100, None, Some("timeout".into()));
        let out = render(&t, 80, 3);
        assert!(
            out.contains("Task · risky · failed"),
            "missing failed label: {out}"
        );
        assert!(out.contains("· Error"), "missing error label: {out}");
        assert!(out.contains("timeout"), "missing error text: {out}");
    }

    #[test]
    fn children_render_with_tree_connectors() {
        let mut t = TaskCell::new_running("tu_parent", "wrap");
        t.push_child_started("tu_a", "bash", "ls");
        t.push_child_started("tu_b", "read_file", "src/main.rs");
        t.push_child_completed("tu_a", "completed", 20);
        let out = render(&t, 80, 5);
        assert!(out.contains("├"), "missing mid-connector: {out}");
        assert!(out.contains("Bash ls · 20ms"), "missing first child: {out}");
        assert!(
            out.contains("Read src/main.rs"),
            "missing calmer running child row: {out}"
        );
    }

    #[test]
    fn collapsed_summary_uses_steps_language() {
        let mut t = TaskCell::new_running("tu_parent", "wrap");
        t.push_child_started("a", "bash", "ls");
        t.push_child_completed("a", "completed", 20);
        t.push_child_started("b", "read_file", "src/main.rs");
        t.push_child_completed("b", "completed", 20);
        t.push_child_started("c", "grep", "TODO");
        t.push_child_completed("c", "completed", 20);
        t.push_child_started("d", "write_file", "notes.md");
        t.push_child_completed("d", "completed", 20);
        t.complete("completed", 100, None, None);

        let out = render(&t, 80, 4);
        assert!(out.contains("4 steps · all done"), "{out}");
        assert!(!out.contains("4 tools"), "{out}");
    }

    #[test]
    fn completed_task_uses_last_branch_connector() {
        let mut t = TaskCell::new_running("tu_parent", "wrap");
        t.push_child_started("tu_a", "bash", "ls");
        t.push_child_completed("tu_a", "completed", 20);
        t.complete("completed", 30, None, None);
        let out = render(&t, 80, 3);
        assert!(
            out.contains("└"),
            "final child should use └ connector when task is terminal: {out}"
        );
    }
}
