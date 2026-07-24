//! Tool-invocation history cell — the live `● Running Bash (42ms)` and
//! terminal `● Ran Bash · 42ms` blocks.
//!
//! Three visual states:
//! - **Running** — accent bullet, shimmer title, elapsed from
//!   construction `Instant`, optional Braille spinner+progress bar
//!   if the tool has been running more than 3 s. Not persisted
//!   until the final `complete()` call.
//! - **Success** — green bullet, `Ran <name> · Xms` title, optional
//!   description (`│ <cmd>`) + output summary (`└ <first 5 lines>`).
//! - **Failed** — red bullet, `Ran <name> · Xms`, otherwise identical to success.
//! - **Rejected** — warning bullet, `Did not run <name> · Xms`; the
//!   runtime rejected the request before execution began
//!
//! Diff-looking output summaries (lines starting with `+` or `-`)
//! get routed through `diff_render` so +/- lines light up green/red
//! with gutters and line numbers. Plain text falls back to a
//! truncated preview.
//!
//! Persists as [`TurnEvent::Tool`] — but **only after completion**.
//! A still-running cell's `to_persist()` returns `None` because
//! the on-disk transcript is the record of committed turns, not a
//! live log.

use std::any::Any;
use std::borrow::Cow;
use std::time::Instant;

use super::HistoryCell;
use super::truncate_by_width;
use crate::cli::cli_config::cli_formatting::extract_cli_diff_block;
use crate::cli::tool_result_status::tool_result_status_is_success;
use crate::tui::render::line_utils::sanitize_terminal_text;
use crate::tui::turn_event::{ToolStatus as PersistStatus, TurnEvent};
use crate::tui::wrapping::{RtOptions, word_wrap_lines};
use astra_tools::exit_semantics::{ExitSemantics, classify_exit};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Live status. `Running` is intentionally separate from the
/// persisted `TurnEvent::Tool.status` enum — a still-running tool
/// never reaches disk, so the schema only carries terminal states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolStatus {
    Running,
    Success,
    /// The execution receipt is incomplete while independent structured
    /// evidence proves that work is present. It must remain distinguishable
    /// from both a successful tool call and a failed one after persistence.
    Uncertain,
    Failed,
    /// The runtime rejected the request before the executor began work.
    /// This is terminal but differs from a tool failure, which happened after
    /// the tool was admitted and attempted.
    Rejected,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCell {
    pub name: String,
    pub description: String,
    pub status: ToolStatus,
    pub started_at: Instant,
    pub duration_ms: Option<u64>,
    pub output_summary: Option<String>,
    pub output: Option<String>,
    pub ts: Option<String>,
    /// Cumulative lines observed in the tool's stdout stream (bash
    /// only today — other tools don't emit `ToolOutput` events so
    /// these stay at 0 and the cell renders an indeterminate
    /// breathing animation instead of a line counter).
    pub progress_lines: u64,
    pub progress_bytes: u64,
    /// Whether the live bash row should advertise Ctrl+B promotion.
    /// This is a UI capability bit supplied by the TUI event loop,
    /// not inferred from the tool name: non-interactive render paths
    /// and edge-less sessions must not promise a shortcut that cannot
    /// work.
    pub ctrl_b_background_hint: bool,
}

impl ToolCell {
    pub fn new_running(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: ToolStatus::Running,
            started_at: Instant::now(),
            duration_ms: None,
            output_summary: None,
            output: None,
            ts: None,
            progress_lines: 0,
            progress_bytes: 0,
            ctrl_b_background_hint: false,
        }
    }

    pub fn set_ctrl_b_background_hint(&mut self, enabled: bool) {
        self.ctrl_b_background_hint = enabled;
    }

    /// Update mid-flight progress counters from a `ToolOutput`
    /// event. Monotonic by contract — callers pass cumulative
    /// values, so we clamp against regressions that could otherwise
    /// reset the on-screen display (e.g. out-of-order delivery).
    pub fn set_progress(&mut self, lines: u64, bytes: u64) {
        if lines >= self.progress_lines {
            self.progress_lines = lines;
        }
        if bytes >= self.progress_bytes {
            self.progress_bytes = bytes;
        }
    }

    /// Transition a Running cell to a terminal state. Idempotent
    /// on re-call (last write wins) — tests + replay paths rely on
    /// this to normalise duplicate completion events.
    pub fn complete(
        &mut self,
        status_str: &str,
        duration_ms: u64,
        description: String,
        output_summary: Option<String>,
        output: Option<String>,
    ) {
        self.status = match status_str {
            "uncertain" => ToolStatus::Uncertain,
            "rejected" => ToolStatus::Rejected,
            _ if tool_result_status_is_success(status_str) => ToolStatus::Success,
            _ => ToolStatus::Failed,
        };
        self.duration_ms = Some(duration_ms);
        if !description.is_empty() {
            self.description = description;
        }
        self.output_summary = non_empty_tool_text(output_summary);
        self.output = non_empty_tool_text(output);
        self.ensure_failure_details();
    }

    /// Resume constructor. Duration is restored verbatim; the
    /// `started_at` Instant is meaningless on reload (we can't
    /// reconstruct a past wall clock) so we pin it to `now() -
    /// duration` so any `elapsed()` call still lines up with the
    /// persisted string.
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        let TurnEvent::Tool {
            ts,
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
        } = ev
        else {
            return None;
        };
        let status = match status {
            PersistStatus::Success => ToolStatus::Success,
            PersistStatus::Uncertain => ToolStatus::Uncertain,
            PersistStatus::Failed => ToolStatus::Failed,
        };
        let started_at = Instant::now()
            .checked_sub(std::time::Duration::from_millis(duration_ms))
            .unwrap_or_else(Instant::now);
        let mut cell = Self {
            name,
            description,
            status,
            started_at,
            duration_ms: Some(duration_ms),
            output_summary: non_empty_tool_text(output_summary),
            output: non_empty_tool_text(output),
            ts,
            progress_lines: 0,
            progress_bytes: 0,
            ctrl_b_background_hint: false,
        };
        cell.ensure_failure_details();
        Some(cell)
    }

    fn ensure_failure_details(&mut self) {
        if self.status != ToolStatus::Failed {
            return;
        }
        if self
            .output_summary
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
            || self
                .output
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
        {
            return;
        }
        let fallback = failure_detail_fallback(&self.name, &self.description);
        self.output_summary = Some(fallback.clone());
        self.output = Some(fallback);
    }

    fn bullet(&self) -> Span<'static> {
        let theme = crate::tui::theme::current();
        match self.status {
            // A solid state dot is easier to scan than a tiny middle dot.
            // Running keeps the focus accent; success/failure use their
            // narrow semantic roles and never recolor the surrounding prose.
            ToolStatus::Running => {
                let theme = crate::tui::theme::current();
                Span::styled("● ", Style::default().fg(theme.accent).bold())
            }
            ToolStatus::Success => Span::styled("● ", Style::default().fg(theme.success).bold()),
            ToolStatus::Uncertain => Span::styled("● ", Style::default().fg(theme.warn).bold()),
            ToolStatus::Failed => Span::styled("● ", Style::default().fg(theme.error).bold()),
            ToolStatus::Rejected => Span::styled("● ", Style::default().fg(theme.warn).bold()),
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

    fn display_name(&self) -> String {
        friendly_tool_display_name_for_context(&self.name, &self.description)
    }

    fn preview_text(&self) -> Option<Cow<'_, str>> {
        if let Some(summary) = self.bash_empty_output_projection() {
            return Some(Cow::Owned(summary));
        }
        if self.name == "task_list"
            && let Some(preview) = self
                .output_summary
                .as_deref()
                .or(self.output.as_deref())
                .and_then(background_task_list_preview)
        {
            return Some(Cow::Owned(preview));
        }
        match (self.output_summary.as_deref(), self.output.as_deref()) {
            (Some(summary), Some(output))
                if !output.trim().is_empty() && is_placeholder_capture_summary(summary) =>
            {
                Some(Cow::Borrowed(output))
            }
            (Some(summary), _) => Some(Cow::Borrowed(summary)),
            (None, Some(output)) => Some(Cow::Borrowed(output)),
            (None, None) => None,
        }
    }

    fn bash_empty_output_projection(&self) -> Option<String> {
        if self.name != "bash" {
            return None;
        }
        let exit_code = bash_exit_code_from_output(self.output.as_deref())?;
        if !bash_output_has_only_exit_metadata(self.output.as_deref()) {
            return None;
        }
        match classify_exit(bash_command_from_description(&self.description), exit_code) {
            ExitSemantics::EmptyResult => Some("no matches".to_string()),
            _ => Some(format!("exit {exit_code} · no output")),
        }
    }

    fn edited_diff_preview(&self) -> Option<EditedDiffPreview<'_>> {
        if !matches!(
            self.name.as_str(),
            "write_file" | "str_replace" | "multi_edit"
        ) {
            return None;
        }

        let diff = self
            .output
            .as_deref()
            .and_then(extract_cli_diff_block)
            .or_else(|| {
                self.preview_text().and_then(|text| {
                    let has_diff = text.lines().any(|line| {
                        line.starts_with("@@")
                            || line.starts_with("--- ")
                            || line.starts_with("+++ ")
                            || line.starts_with('+')
                            || line.starts_with('-')
                    });
                    has_diff.then_some(text)
                })
            })?;

        let additions = diff
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++ "))
            .count();
        let deletions = diff
            .lines()
            .filter(|line| line.starts_with('-') && !line.starts_with("--- "))
            .count();
        let files: Vec<&str> = diff
            .lines()
            .filter_map(|line| line.strip_prefix("+++ b/"))
            .filter(|path| !path.is_empty() && *path != "/dev/null")
            .collect();
        let label = if files.len() == 1 {
            files[0].to_string()
        } else if files.len() > 1 {
            format!("{} files", files.len())
        } else if !self.description.trim().is_empty() {
            self.description.trim().to_string()
        } else {
            self.display_name()
        };

        Some(EditedDiffPreview {
            label,
            additions,
            deletions,
            diff,
        })
    }

    /// Sub-line rendered under the header for tools that are still
    /// running past the 3 s grace window.
    ///
    /// Two shapes:
    /// - **Signal mode** (any `progress_lines` / `progress_bytes`
    ///   arrived) — a Braille spinner + `"streaming · N lines · K KB"`
    ///   counter. Honest, monotonic, no fake percentages.
    /// - **Indeterminate mode** (no progress signal ever arrived —
    ///   non-streaming tools like `read_file`, `git(action=log)`, skill
    ///   dispatch) — a breathing bar with a small block sliding back
    ///   and forth. Purely time-based; makes "still working" visible
    ///   without pretending to track progress.
    fn progress_line(&self, width: usize, elapsed_ms: u64) -> Line<'static> {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame_idx = ((elapsed_ms / 80) % FRAMES.len() as u64) as usize;
        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(crate::tui::theme::current().dim);
        let spinner = Span::styled(
            FRAMES[frame_idx].to_string(),
            Style::default().fg(theme.accent),
        );

        // Signal mode: show real counters when the tool actually
        // streamed something.
        let background_hint = if self.name == "bash" && self.ctrl_b_background_hint {
            " · Ctrl+B to background"
        } else {
            ""
        };
        if self.progress_lines > 0 || self.progress_bytes > 0 {
            let body = format!(
                " streaming · {} {} · {}{}",
                self.progress_lines,
                if self.progress_lines == 1 {
                    "line"
                } else {
                    "lines"
                },
                format_bytes(self.progress_bytes),
                background_hint,
            );
            return Line::from(vec![
                Span::styled("    ", dim),
                spinner,
                Span::styled(body, dim),
            ]);
        }

        // Indeterminate mode: breathing block slides across a
        // fixed-width track. Position is `t = elapsed_ms / 1400`,
        // normalised to [0, 1] via a triangle wave so the block
        // bounces off the ends rather than wrapping.
        let bar_max = width.saturating_sub(12).clamp(10, 28);
        let block_len = (bar_max / 4).max(2);
        let travel = bar_max.saturating_sub(block_len);
        let pos = if travel == 0 {
            0
        } else {
            let t = (elapsed_ms as f32 / 1400.0).fract();
            let tri = if t < 0.5 { t * 2.0 } else { (1.0 - t) * 2.0 };
            (tri * travel as f32).round() as usize
        };
        let mut bar = String::with_capacity(bar_max);
        for i in 0..bar_max {
            if i >= pos && i < pos + block_len {
                bar.push('▓');
            } else {
                bar.push('░');
            }
        }

        Line::from(vec![
            Span::styled("    ", dim),
            spinner,
            Span::raw(" "),
            Span::styled(bar, Style::default().fg(theme.accent)),
            Span::styled(background_hint.to_string(), dim),
        ])
    }
}

/// Human-readable byte count (1024-base). Three significant digits
/// up through GB — matches what `ls -h` feels like.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    }
}

impl ToolCell {
    /// True when the compact tool row omits arguments or result content that
    /// the transcript can reveal. This is based on semantic payloads rather
    /// than rendered error-message matching.
    pub(crate) fn has_transcript_details(&self) -> bool {
        if self.description.lines().count() > 2 {
            return true;
        }
        if self
            .edited_diff_preview()
            .is_some_and(|edited| edited.diff.lines().count() > 12)
        {
            return true;
        }

        let preview = self.preview_text().filter(|text| !text.trim().is_empty());
        let detail = self
            .output
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                self.output_summary
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
            });
        match (preview.as_deref(), detail) {
            (_, Some(detail))
                if detail.lines().count()
                    > if looks_like_unified_diff_preview(detail) {
                        10
                    } else {
                        5
                    } =>
            {
                true
            }
            (Some(preview), Some(detail)) => preview.trim() != detail.trim(),
            (None, Some(_)) => true,
            _ => false,
        }
    }

    /// Whether the runtime supplied a typed receipt proving that execution
    /// never began. Tool failures normally represent attempted execution, but
    /// malformed provider arguments fail before that boundary and must not be
    /// described as `Ran ...`.
    fn execution_was_rejected(&self) -> bool {
        if self.status == ToolStatus::Rejected {
            return true;
        }
        [self.output_summary.as_deref(), self.output.as_deref()]
            .into_iter()
            .flatten()
            .any(|text| {
                structured_tool_result(text).is_some_and(|value| {
                    value
                        .pointer("/advisory/executed")
                        .and_then(serde_json::Value::as_bool)
                        == Some(false)
                }) || is_deferred_admission_rejection(text)
            })
    }

    /// Transcript-only projection with optional full arguments/result. The
    /// canonical Tool event remains unchanged.
    pub(crate) fn transcript_lines(&self, width: u16, expanded: bool) -> Vec<Line<'static>> {
        let has_details = self.has_transcript_details();
        let mut lines = self.display_lines_with_details(width, expanded && has_details);
        if has_details && let Some(header) = lines.first_mut() {
            let marker = if expanded { "▼ " } else { "▶ " };
            if let Some(status_marker) = header.spans.first_mut() {
                status_marker.content = Cow::Borrowed(marker);
            }
        }
        lines
    }

    fn detailed_text(&self) -> Option<&str> {
        self.output
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                self.output_summary
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
            })
    }

    fn display_lines_with_details(&self, width: u16, expanded: bool) -> Vec<Line<'static>> {
        let dim = Style::default().dim();
        let w = width as usize;

        let label = self.display_name();
        let theme = crate::tui::theme::current();
        let meta_style = Style::default().fg(theme.dim);
        let edited_diff = if self.status == ToolStatus::Running {
            None
        } else {
            self.edited_diff_preview()
        };
        let preview_text = if expanded {
            self.detailed_text().map(Cow::Borrowed)
        } else {
            self.preview_text()
        }
        .filter(|text| !text.trim().is_empty());
        let header = if let Some(edited) = edited_diff.as_ref() {
            let mut spans = vec![
                self.bullet(),
                Span::styled("Edited ", Style::default().bold()),
            ];
            // Use path styling for file paths — directory dim, filename bright.
            if edited.label.contains('/') || edited.label.contains('\\') {
                let truncated = truncate_by_width(&edited.label, w.saturating_sub(24).max(12));
                spans.extend(crate::tui::path_style::style_file_path_flat(
                    &truncated,
                    Style::default(),
                ));
            } else {
                spans.push(Span::styled(
                    truncate_by_width(&edited.label, w.saturating_sub(24).max(12)),
                    Style::default(),
                ));
            }
            if edited.additions > 0 || edited.deletions > 0 {
                spans.push(Span::styled(" · ", meta_style));
                spans.push(Span::styled(
                    format!("+{}", edited.additions),
                    Style::default().fg(theme.success),
                ));
                spans.push(Span::styled(" ", meta_style));
                spans.push(Span::styled(
                    format!("-{}", edited.deletions),
                    Style::default().fg(theme.error),
                ));
            }
            Line::from(spans)
        } else if self.status == ToolStatus::Running {
            let text = if let Some(task_header) = background_task_tool_header(&self.name) {
                format!("{task_header} ({})", self.elapsed_str())
            } else {
                format!("Running {label} ({})", self.elapsed_str())
            };
            let mut spans = vec![self.bullet()];
            spans.extend(crate::tui::shimmer::shimmer_spans(&text));
            Line::from(spans)
        } else {
            let title = if self.execution_was_rejected() {
                format!("Did not run {label}")
            } else if let Some(task_header) = background_task_tool_header(&self.name) {
                task_header.to_string()
            } else {
                format!("Ran {label}")
            };
            let spans = vec![
                self.bullet(),
                Span::styled(title, Style::default().bold()),
                Span::styled(" · ", meta_style),
                Span::styled(self.elapsed_str(), meta_style),
            ];
            Line::from(spans)
        };

        let mut lines = vec![header];

        let missing_failure_details =
            self.status == ToolStatus::Failed && edited_diff.is_none() && preview_text.is_none();
        let has_preview_block = edited_diff.is_some() || preview_text.is_some();
        let description_has_children = missing_failure_details || has_preview_block;

        // Spinner + progress bar for long-running tools.
        if self.status == ToolStatus::Running {
            let elapsed = self.started_at.elapsed().as_millis() as u64;
            if elapsed >= 3_000 {
                lines.push(self.progress_line(w, elapsed));
            }
        }

        // Command/path line with a light structural guide.
        if edited_diff.is_none() && !self.description.is_empty() {
            let description = sanitize_terminal_text(&self.description);
            let theme = crate::tui::theme::current();
            let command_style = theme.command_style();
            let desc_prefix = if description_has_children {
                "  ├ "
            } else {
                "  └ "
            };
            let desc_indent = if description_has_children {
                "  │ "
            } else {
                "    "
            };
            let description_limit = if expanded { usize::MAX } else { 2 };
            for dl in description.lines().take(description_limit) {
                let mut spans = vec![Span::styled(desc_prefix, dim)];
                if self.name == "bash" {
                    if let Some(cmd) = dl.strip_prefix("$ ") {
                        spans.push(Span::styled("$ ".to_string(), dim));
                        // Split command name (bold + colour) from arguments (dim).
                        if let Some((first, rest)) = cmd.split_once(' ') {
                            spans.push(Span::styled(first.to_string(), command_style.bold()));
                            spans.push(Span::styled(format!(" {rest}"), command_style));
                        } else {
                            spans.push(Span::styled(cmd.to_string(), command_style.bold()));
                        }
                    } else {
                        spans.push(Span::styled(dl.to_string(), command_style));
                    }
                } else {
                    spans.push(Span::raw(dl.to_string()));
                }
                lines.extend(wrap_prefixed_line(
                    Line::from(spans),
                    width,
                    Line::from(vec![Span::styled(desc_indent, dim)]),
                ));
            }
        }

        // Output summary — diff renderer for +/- content,
        // plain truncated preview otherwise.
        if missing_failure_details {
            let fallback = failure_detail_fallback(&self.name, &self.description);
            let detail = sanitize_terminal_text(&fallback);
            let detail_lines: Vec<&str> = detail.lines().collect();
            for (i, detail_line) in detail_lines.iter().enumerate() {
                let is_last = i + 1 == detail_lines.len();
                let gutter = if is_last { "  └ " } else { "  ├ " };
                lines.extend(wrap_prefixed_line(
                    Line::from(vec![
                        Span::styled(gutter.to_string(), dim),
                        Span::styled((*detail_line).to_string(), Style::default().fg(theme.error)),
                    ]),
                    width,
                    Line::from(vec![Span::styled("  │ ".to_string(), dim)]),
                ));
            }
        }

        if let Some(edited) = edited_diff {
            let diff_limit = if expanded {
                edited.diff.lines().count().max(1)
            } else {
                12
            };
            let diff_lines = crate::tui::diff_render::render_diff_lines(&edited.diff, diff_limit);
            for (i, dl) in diff_lines.into_iter().enumerate() {
                let prefix = if i == 0 { "  └ " } else { "    " };
                let prefixed = prefix_tool_output_line(prefix, dl, dim);
                lines.extend(
                    wrap_prefixed_diff_line(prefixed, width)
                        .into_iter()
                        .map(mark_full_row_background),
                );
            }
        } else if let Some(summary) = preview_text {
            let summary = sanitize_terminal_text(&summary);
            let has_diff = looks_like_unified_diff_preview(&summary);
            if has_diff {
                let diff_limit = if expanded {
                    summary.lines().count().max(1)
                } else {
                    10
                };
                let diff_lines = crate::tui::diff_render::render_diff_lines(&summary, diff_limit);
                for (i, dl) in diff_lines.into_iter().enumerate() {
                    let prefix = if i == 0 { "  └ " } else { "    " };
                    let prefixed = prefix_tool_output_line(prefix, dl, dim);
                    lines.extend(
                        wrap_prefixed_diff_line(prefixed, width)
                            .into_iter()
                            .map(mark_full_row_background),
                    );
                }
            } else {
                let output_limit = if expanded { usize::MAX } else { 5 };
                let out_lines: Vec<&str> = summary.lines().take(output_limit).collect();
                let has_more = !expanded && summary.lines().count() > 5;
                let visible_count = out_lines.len();
                for (i, ol) in out_lines.iter().enumerate() {
                    let is_last_visible = i + 1 == visible_count;
                    let gutter = if is_last_visible && !has_more {
                        "  └ ".to_string()
                    } else {
                        "  ├ ".to_string()
                    };
                    // Style git diff --stat lines: dim path, bright filename.
                    // Also detect bare file paths in plain output lines.
                    let styled_content: Vec<Span<'static>> =
                        if let Some(file_part) = ol.trim().split_once('|') {
                            let path = file_part.0.trim();
                            let stats = file_part.1; // includes leading "| "
                            let mut spans = Vec::new();
                            spans.extend(crate::tui::path_style::style_file_path(path));
                            spans.push(Span::styled(format!(" |{stats}"), dim));
                            spans
                        } else {
                            let trimmed = ol.trim();
                            if looks_like_file_path(trimmed) {
                                crate::tui::path_style::style_file_path(trimmed)
                            } else {
                                vec![Span::raw((*ol).to_string())]
                            }
                        };
                    let mut line_spans = vec![Span::styled(gutter.clone(), dim)];
                    line_spans.extend(styled_content);
                    lines.extend(wrap_prefixed_line(
                        Line::from(line_spans),
                        width,
                        Line::from(vec![Span::styled("  │ ".to_string(), dim)]),
                    ));
                }
                if has_more {
                    let remaining = summary.lines().count() - 5;
                    lines.push(Line::from(vec![
                        Span::styled("  └ ".to_string(), dim),
                        Span::styled(
                            format!("… +{remaining} lines (Ctrl+O to view transcript)"),
                            dim,
                        ),
                    ]));
                }
            }
        }

        lines
    }
}

impl HistoryCell for ToolCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines_with_details(width, false)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn is_live(&self) -> bool {
        self.status == ToolStatus::Running
    }

    fn finalize(&mut self) {
        // `complete()` is the canonical terminal transition. If
        // `finalize()` fires on a still-Running cell (e.g. turn
        // got interrupted mid-tool), mark it Failed so the
        // persisted record reflects the outcome rather than
        // silently dropping.
        if self.status == ToolStatus::Running {
            self.status = ToolStatus::Failed;
            if self.duration_ms.is_none() {
                self.duration_ms = Some(self.started_at.elapsed().as_millis() as u64);
            }
        }
        self.output_summary = non_empty_tool_text(self.output_summary.take());
        self.output = non_empty_tool_text(self.output.take());
        self.ensure_failure_details();
    }

    fn to_persist(&self) -> Option<TurnEvent> {
        // Running cells never hit disk — the journal records
        // committed turns only. `finalize()` / `complete()` must
        // run first.
        let status = match self.status {
            ToolStatus::Success => PersistStatus::Success,
            ToolStatus::Uncertain => PersistStatus::Uncertain,
            ToolStatus::Failed | ToolStatus::Rejected => PersistStatus::Failed,
            ToolStatus::Running => return None,
        };
        Some(TurnEvent::Tool {
            ts: self.ts.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            status,
            duration_ms: self.duration_ms.unwrap_or(0),
            output_summary: self.output_summary.clone(),
            output: self.output.clone(),
        })
    }
}

/// Backwards-compatible recognition for journals written before the stream
/// carried the typed `rejected` status. This is a protocol message, not a
/// tool-name exception: it applies to every deferred capability.
fn is_deferred_admission_rejection(text: &str) -> bool {
    text.starts_with("Error: Tool '")
        && text.contains("' is not available in this turn yet.")
        && text.contains("<deferred-tools>")
}

fn prefix_tool_output_line(prefix: &str, line: Line<'static>, fallback: Style) -> Line<'static> {
    let bg = line
        .spans
        .iter()
        .find_map(|span| span.style.bg)
        .unwrap_or(Color::Reset);
    let mut spans = vec![Span::styled(prefix.to_string(), fallback.bg(bg))];
    spans.extend(line.spans);
    Line::from(spans)
}

fn wrap_prefixed_line(
    line: Line<'static>,
    width: u16,
    subsequent_indent: Line<'static>,
) -> Vec<Line<'static>> {
    wrap_prefixed_line_hard(line, width, subsequent_indent)
}

fn wrap_prefixed_diff_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    if line.width() <= usize::from(width) {
        return vec![line];
    }
    let indent = if line.spans.len() >= 3 {
        Line::from(vec![
            blank_span_like(&line.spans[0]),
            blank_span_like(&line.spans[1]),
            blank_span_like(&line.spans[2]),
        ])
    } else if let Some(first) = line.spans.first() {
        Line::from(vec![blank_span_like(first)])
    } else {
        Line::from("    ")
    };
    wrap_prefixed_line_hard(line, width, indent)
}

fn wrap_prefixed_line_hard(
    line: Line<'static>,
    width: u16,
    subsequent_indent: Line<'static>,
) -> Vec<Line<'static>> {
    word_wrap_lines(
        [line],
        RtOptions::new(width as usize)
            .subsequent_indent(subsequent_indent)
            .word_separator(textwrap::WordSeparator::AsciiSpace)
            .word_splitter(textwrap::WordSplitter::Custom(split_every_char))
            .break_words(false),
    )
}

fn split_every_char(word: &str) -> Vec<usize> {
    word.char_indices().skip(1).map(|(idx, _)| idx).collect()
}

fn blank_span_like(span: &Span<'_>) -> Span<'static> {
    Span::styled(
        " ".repeat(UnicodeWidthStr::width(span.content.as_ref())),
        span.style,
    )
}

fn mark_full_row_background(mut line: Line<'static>) -> Line<'static> {
    let Some(bg) = line.spans.iter().find_map(|span| span.style.bg) else {
        return line;
    };
    // Full-row colour is presentation metadata, not content. Padding a line
    // with spaces to the terminal width leaves a real terminal in auto-wrap
    // pending state; the next CRLF can then create a phantom blank row. Each
    // renderer owns the physical surface (FullRowParagraph for buffers,
    // erase-to-end-of-line for scrollback), while wrapping continues to see
    // only content.
    line.style = line.style.bg(bg);
    line
}

fn is_placeholder_capture_summary(summary: &str) -> bool {
    let normalized = summary.trim().to_ascii_lowercase();
    normalized.ends_with("lines captured")
        || normalized.ends_with("output lines captured")
        || normalized.ends_with("file lines read")
}

fn looks_like_unified_diff_preview(text: &str) -> bool {
    let mut saw_hunk = false;
    let mut saw_file_headers = false;
    let mut saw_change = false;
    let mut saw_inline_headers_only = true;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if line.starts_with("@@") {
            saw_hunk = true;
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            saw_file_headers = true;
            continue;
        }
        if (line.starts_with('+') && !line.starts_with("+++ "))
            || (line.starts_with('-') && !line.starts_with("--- "))
        {
            saw_change = true;
            continue;
        }
        if line.starts_with("… +") && line.contains("more changed lines") {
            continue;
        }
        if !looks_like_inline_diff_header(trimmed) {
            saw_inline_headers_only = false;
        }
    }

    saw_change && (saw_hunk || saw_file_headers || saw_inline_headers_only)
}

fn looks_like_inline_diff_header(line: &str) -> bool {
    !line.contains(char::is_whitespace)
        && !line.contains(':')
        && (line.contains('.') || line.contains('/') || line.contains('\\'))
}

pub(super) fn humanize_tool_name(name: &str) -> String {
    let mut out = String::new();
    for (i, part) in name.split('_').filter(|part| !part.is_empty()).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    if out.is_empty() {
        name.to_string()
    } else {
        out
    }
}

fn non_empty_tool_text(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

fn structured_tool_result(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok().or_else(|| {
        // The model-facing recovery hint may follow the canonical JSON receipt
        // on a separate line. Parse only the complete first record; never
        // infer lifecycle state from human-readable recovery prose.
        serde_json::from_str(text.lines().next()?).ok()
    })
}

fn failure_detail_fallback(name: &str, description: &str) -> String {
    let label = friendly_tool_display_name_for_context(name, description);
    if name == "agent_fanout" {
        return "Fanout did not return a usable launch receipt.\nThe launch outcome is unconfirmed; Shift+↓ shows observed background work.".into();
    }
    let description = description.trim();
    if description.is_empty() {
        format!(
            "{label} ended without a result payload.\n\
             The execution outcome could not be confirmed."
        )
    } else {
        format!(
            "{label} ended without a result payload: {description}.\n\
             The execution outcome could not be confirmed."
        )
    }
}

fn friendly_tool_display_name_for_context(name: &str, _description: &str) -> String {
    friendly_tool_display_name(name)
}

fn bash_command_from_description(description: &str) -> &str {
    description
        .trim()
        .strip_prefix("$ ")
        .unwrap_or(description.trim())
}

fn bash_exit_code_from_output(output: Option<&str>) -> Option<i32> {
    let output = output?.trim();
    let marker = output
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("[exit code: "))?;
    marker.strip_suffix(']')?.parse().ok()
}

fn bash_output_has_only_exit_metadata(output: Option<&str>) -> bool {
    let Some(output) = output else {
        return false;
    };
    output.lines().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty() || (trimmed.starts_with("[exit code: ") && trimmed.ends_with(']'))
    })
}

fn friendly_tool_display_name(name: &str) -> String {
    match name {
        "bash" => "Bash".into(),
        "read" | "read_file" => "Read".into(),
        "write_file" => "Write file".into(),
        "str_replace" => "Replace text".into(),
        "grep" | "glob" => "Search".into(),
        "list_dir" => "List directory".into(),
        "task_board" => "Task".into(),
        "memory" => "Memory".into(),
        "tool_search" => "Tool search".into(),
        _ => humanize_tool_name(name),
    }
}

fn background_task_tool_header(name: &str) -> Option<&'static str> {
    match name {
        "task_output" => Some("Read background task output"),
        "task_stop" => Some("Stop background task"),
        "task_list" => Some("List background tasks"),
        _ => None,
    }
}

fn background_task_list_preview(xml: &str) -> Option<String> {
    let text = xml.trim();
    if !text.starts_with("<background_tasks") {
        return None;
    }
    let count = xml_attr(text, "count")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if count == 0 {
        return Some("No background tasks.".to_string());
    }

    let mut lines = vec![format!(
        "{} background {}",
        count,
        if count == 1 { "task" } else { "tasks" }
    )];
    for task in text.match_indices("<task ").take(4).map(|(idx, _)| {
        let rest = &text[idx..];
        rest.split_once("/>").map(|(tag, _)| tag).unwrap_or(rest)
    }) {
        let id = xml_attr(task, "id").unwrap_or_else(|| "?".to_string());
        let kind = xml_attr(task, "kind").unwrap_or_else(|| "task".to_string());
        let status = xml_attr(task, "status").unwrap_or_else(|| "unknown".to_string());
        let elapsed = xml_attr(task, "elapsed_ms")
            .and_then(|ms| ms.parse::<u64>().ok())
            .map(format_elapsed_ms_compact)
            .unwrap_or_else(|| "-".to_string());
        let command = xml_attr(task, "command").unwrap_or_default();
        let command = if command.is_empty() {
            String::new()
        } else {
            format!("  {command}")
        };
        lines.push(format!("{id}  {kind}  {status}  {elapsed}{command}"));
    }
    if count > 4 {
        lines.push(format!("… +{} more", count - 4));
    }
    Some(lines.join("\n"))
}

fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(xml_unescape(&rest[..end]))
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn format_elapsed_ms_compact(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{mins}m{secs}s")
    }
}

/// Quick heuristic: does `s` look like a file path that should be
/// rendered with dimmed directory / bright filename styling?
fn looks_like_file_path(s: &str) -> bool {
    // Must contain a path separator and a dot (likely an extension).
    if !s.contains('/') && !s.contains('\\') {
        return false;
    }
    // Reject URLs and flag-like strings.
    if s.starts_with("http://") || s.starts_with("https://") || s.starts_with("--") {
        return false;
    }
    // Must have a dot after the last separator (an extension).
    let last_sep = s.rfind('/').or_else(|| s.rfind('\\')).unwrap_or(0);
    let after_sep = &s[last_sep..];
    after_sep.contains('.')
}

struct EditedDiffPreview<'a> {
    label: String,
    additions: usize,
    deletions: usize,
    diff: Cow<'a, str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::render::line_utils::sanitize_lines_for_terminal;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &ToolCell, width: u16, height: u16) -> String {
        let lines = sanitize_lines_for_terminal(cell.display_lines(width));
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, height))
    }

    fn ok_tool(name: &str, desc: &str, dur: u64) -> ToolCell {
        let mut t = ToolCell::new_running(name, desc);
        t.status = ToolStatus::Success;
        t.duration_ms = Some(dur);
        t
    }

    fn err_tool(name: &str, desc: &str, dur: u64) -> ToolCell {
        let mut t = ToolCell::new_running(name, desc);
        t.status = ToolStatus::Failed;
        t.duration_ms = Some(dur);
        t
    }

    // ── Live state ───────────────────────────────────────────────

    #[test]
    fn running_is_live_completed_is_not() {
        let running = ToolCell::new_running("bash", "");
        assert!(running.is_live(), "running cell must be live");

        let done = ok_tool("bash", "", 42);
        assert!(!done.is_live());
    }

    #[test]
    fn complete_transitions_out_of_running() {
        let mut t = ToolCell::new_running("bash", "ls");
        t.complete(
            "completed",
            42,
            String::new(),
            Some("3 entries".into()),
            None,
        );
        assert_eq!(t.status, ToolStatus::Success);
        assert_eq!(t.duration_ms, Some(42));
        assert_eq!(t.output_summary.as_deref(), Some("3 entries"));
    }

    #[test]
    fn complete_accepts_success_aliases_from_tool_output() {
        let mut t = ToolCell::new_running("bash", "ls");
        t.complete("ok", 42, String::new(), Some("3 entries".into()), None);
        assert_eq!(t.status, ToolStatus::Success);
    }

    #[test]
    fn uncertain_tool_outcome_survives_persistence_roundtrip() {
        let mut live = ToolCell::new_running("agent_fanout", "review in parallel");
        live.complete(
            "uncertain",
            42,
            String::new(),
            Some("a canonical run is visible".into()),
            None,
        );

        let persisted = live.to_persist().expect("settled tool persists");
        let restored = ToolCell::from_persist(persisted).expect("persisted tool restores");
        assert_eq!(restored.status, ToolStatus::Uncertain);
        assert_eq!(
            restored.output_summary.as_deref(),
            Some("a canonical run is visible")
        );
    }

    #[test]
    fn finalize_demotes_stuck_running_to_failed() {
        // If a turn aborts mid-tool, finalize should still produce
        // a persistable record rather than silently losing the row.
        let mut t = ToolCell::new_running("bash", "slow op");
        t.finalize();
        assert_eq!(t.status, ToolStatus::Failed);
        assert!(t.duration_ms.is_some(), "duration snapshotted on finalize");
        assert_eq!(
            t.output_summary.as_deref(),
            Some(
                "Bash ended without a result payload: slow op.\nThe execution outcome could not be confirmed."
            )
        );
        assert_eq!(t.output.as_deref(), t.output_summary.as_deref());
        assert!(!t.is_live());
    }

    // ── Persistence ──────────────────────────────────────────────

    #[test]
    fn running_does_not_persist() {
        let t = ToolCell::new_running("bash", "sleep 10");
        assert!(t.to_persist().is_none(), "live cells must stay off disk");
    }

    #[test]
    fn persist_roundtrip_success_and_failure() {
        for base in [
            ok_tool("bash", "ls /tmp", 42),
            err_tool("read", "missing.rs", 120),
        ] {
            let persisted = base.to_persist().expect("must persist");
            let back = ToolCell::from_persist(persisted).expect("from_persist");
            assert_eq!(back.status, base.status);
            assert_eq!(back.name, base.name);
            assert_eq!(back.description, base.description);
            assert_eq!(back.duration_ms, base.duration_ms);
        }
    }

    #[test]
    fn from_persist_rejects_wrong_variant() {
        let wrong = TurnEvent::User {
            ts: None,
            text: "x".into(),
        };
        assert!(ToolCell::from_persist(wrong).is_none());
    }

    // ── Render ───────────────────────────────────────────────────

    #[test]
    fn success_header_uses_compact_title_and_meta() {
        let t = ok_tool("bash", "ls /tmp", 42);
        let out = render(&t, 80, 3);
        assert!(
            out.contains("● Ran Bash · 42ms"),
            "unexpected header: {out}"
        );
        assert!(out.contains("└ ls /tmp"));
    }

    #[test]
    fn running_header_uses_present_tense_lifecycle() {
        let t = ToolCell::new_running("agent_fanout", "agent_fanout");
        let out = render(&t, 80, 3);
        assert!(out.contains("Running Agent Fanout"), "{out}");
        assert!(!out.contains("Ran Agent Fanout"), "{out}");
    }

    #[test]
    fn failed_header_uses_red_bullet_without_status_word() {
        let t = err_tool("bash", "false", 10);
        let out = render(&t, 80, 3);
        assert!(out.contains("● Ran Bash · 10ms"));
        assert!(!out.contains("· failed"), "{out}");
    }

    #[test]
    fn pre_execution_rejection_is_not_rendered_as_ran() {
        let mut t = err_tool("agent_fanout", "agent_fanout", 10);
        t.output = Some(
            serde_json::json!({
                "status": "failed",
                "error_kind": "tool_invalid_args",
                "advisory": {"executed": false}
            })
            .to_string(),
        );
        let out = render(&t, 100, 4);
        assert!(out.contains("Did not run Agent Fanout · 10ms"), "{out}");
        assert!(!out.contains("Ran Agent Fanout"), "{out}");
    }

    #[test]
    fn typed_rejection_is_rendered_and_legacy_replay_preserves_its_meaning() {
        let legacy_output = "Error: Tool 'remote_catalog' is not available in this turn yet. It appears in <deferred-tools>.";
        let mut live = ToolCell::new_running("remote_catalog", "list entries");
        live.complete(
            "rejected",
            1,
            String::new(),
            Some(legacy_output.to_string()),
            None,
        );
        assert_eq!(live.status, ToolStatus::Rejected);
        let live_output = render(&live, 100, 4);
        assert!(
            live_output.contains("Did not run Remote Catalog · 1ms"),
            "{live_output}"
        );

        let replay = ToolCell::from_persist(
            live.to_persist()
                .expect("terminal rejected cell must remain journaled"),
        )
        .expect("persisted tool cell");
        let replay_output = render(&replay, 100, 4);
        assert!(
            replay_output.contains("Did not run Remote Catalog · 1ms"),
            "{replay_output}"
        );
        assert!(
            !replay_output.contains("Ran Remote Catalog"),
            "{replay_output}"
        );
    }

    #[test]
    fn bash_description_promotes_command_text() {
        let t = ok_tool("bash", "$ git diff --stat", 42);
        let lines = t.display_lines(80);
        let desc = &lines[1];
        assert_eq!(desc.spans[1].content.as_ref(), "$ ");
        assert_eq!(
            desc.spans[2].style.fg,
            Some(crate::tui::theme::current().command)
        );
    }

    #[test]
    fn failed_tool_without_summary_falls_back_to_output_preview() {
        let mut t = err_tool("bash", "cat missing.txt", 10);
        t.output = Some("cat: missing.txt: No such file or directory".into());
        let out = render(&t, 80, 5);
        assert!(!out.contains("Details in transcript"), "{out}");
        assert!(out.contains("No such file or directory"), "{out}");
        assert!(!out.contains("Ctrl+O transcript"), "{out}");
    }

    #[test]
    fn failed_tool_without_any_details_says_so_explicitly() {
        let t = err_tool("read", "Reading: missing.txt", 10);
        let out = render(&t, 80, 4);
        assert!(
            out.contains("Read ended without a result payload: Reading: missing.txt"),
            "{out}"
        );
        assert!(!out.contains("No details returned"), "{out}");
    }

    #[test]
    fn complete_failed_tool_synthesizes_missing_details() {
        let mut t = ToolCell::new_running("bash", "$ make check 2>&1");
        t.complete("failed", 1200, String::new(), None, None);
        let summary = t.output_summary.as_deref().unwrap();
        assert!(
            summary.contains("Bash ended without a result payload: $ make check 2>&1"),
            "{summary}"
        );
        assert!(
            summary.contains("outcome could not be confirmed"),
            "{summary}"
        );
        assert_eq!(t.output.as_deref(), t.output_summary.as_deref());
        let out = render(&t, 100, 4);
        assert!(out.contains("Bash ended without a result payload"), "{out}");
        assert!(out.contains("outcome could not be confirmed"), "{out}");
        assert!(!out.contains("No details returned"), "{out}");
    }

    #[test]
    fn fanout_without_receipt_points_to_live_agent_observation() {
        let mut t = ToolCell::new_running("agent_fanout", "start parallel review");
        t.complete("failed", 12, String::new(), None, None);

        let out = render(&t, 100, 4);
        assert!(
            out.contains("Fanout did not return a usable launch receipt"),
            "{out}"
        );
        assert!(out.contains("Shift+↓"), "{out}");
        assert!(!out.contains("agent_tool_reporting_error"), "{out}");
        assert!(!out.contains("failed before returning output"), "{out}");
    }

    #[test]
    fn bash_search_exit_one_with_no_output_renders_no_matches() {
        let mut t = ToolCell::new_running("bash", "$ rg definitely_missing_token src");
        t.complete(
            "completed",
            28,
            String::new(),
            Some("1 line captured".into()),
            Some("[exit code: 1]".into()),
        );
        let out = render(&t, 100, 5);
        assert!(out.contains("no matches"), "{out}");
        assert!(!out.contains("1 line captured"), "{out}");
        assert!(!out.contains("failed before returning output"), "{out}");
        assert!(!out.contains("No details returned"), "{out}");
    }

    #[test]
    fn bash_cd_wrapped_grep_exit_one_with_no_output_renders_no_matches() {
        let mut t = ToolCell::new_running(
            "bash",
            "$ cd /work/repo && grep -n definitely_missing_token src/main.rs",
        );
        t.complete(
            "completed",
            28,
            String::new(),
            Some("[exit code: 1]".into()),
            Some("[exit code: 1]".into()),
        );
        let out = render(&t, 120, 5);
        assert!(out.contains("no matches"), "{out}");
        assert!(!out.contains("exit 1 · no output"), "{out}");
        assert!(!out.contains("No details returned"), "{out}");
    }

    #[test]
    fn bash_non_search_exit_with_no_output_renders_exit_code() {
        let mut t = ToolCell::new_running("bash", "$ cd /missing && grep x file");
        t.complete(
            "failed",
            28,
            String::new(),
            Some("[exit code: 2]".into()),
            Some("[exit code: 2]".into()),
        );
        let out = render(&t, 100, 5);
        assert!(out.contains("exit 2 · no output"), "{out}");
        assert!(!out.contains("failed before returning output"), "{out}");
        assert!(!out.contains("No details returned"), "{out}");
    }

    #[test]
    fn task_output_uses_typed_history_header_not_generic_ran_tool() {
        let mut t = ok_tool("task_output", "task_id=bg-shell-1", 28);
        t.output_summary = Some(
            "Read shell output bg-shell-1\n2 new lines · offset 0 -> 42 · total 42 bytes · still running"
                .to_string(),
        );
        let out = render(&t, 120, 5);
        assert!(out.contains("Read background task output · 28ms"), "{out}");
        assert!(out.contains("Read shell output bg-shell-1"), "{out}");
        assert!(!out.contains("Ran Task output"), "{out}");
        assert!(!out.contains("Ran Read background task output"), "{out}");
    }

    #[test]
    fn task_stop_uses_typed_history_header_not_generic_ran_tool() {
        let mut t = ok_tool("task_stop", "task_id=bg-shell-2", 31);
        t.output_summary = Some("Background task bg-shell-2 stopped.".to_string());
        let out = render(&t, 120, 4);
        assert!(out.contains("Stop background task · 31ms"), "{out}");
        assert!(out.contains("Background task bg-shell-2 stopped."), "{out}");
        assert!(!out.contains("Ran Task stop"), "{out}");
    }

    #[test]
    fn task_list_uses_typed_history_header_not_generic_ran_tool() {
        let mut t = ok_tool("task_list", "", 19);
        t.output_summary = Some(
            "<background_tasks count=\"1\">\n<task id=\"bg-shell-1\" kind=\"shell\" status=\"running\" live_control=\"available\" elapsed_ms=\"1200\" command=\"cargo test &amp;&amp; echo ok\" output_ref=\"stdout: /tmp/bg-shell-1.stdout\" output_offset=\"0\" total_output_bytes=\"12\" total_output_lines=\"1\" preview=\"ok\" />\n</background_tasks>"
                .to_string(),
        );
        let out = render(&t, 140, 5);
        assert!(out.contains("List background tasks · 19ms"), "{out}");
        assert!(out.contains("1 background task"), "{out}");
        assert!(out.contains("bg-shell-1  shell  running  1.2s"), "{out}");
        assert!(out.contains("cargo test && echo ok"), "{out}");
        assert!(!out.contains("<background_tasks"), "{out}");
        assert!(!out.contains("live_control"), "{out}");
        assert!(!out.contains("output_ref"), "{out}");
        assert!(!out.contains("Ran Task list"), "{out}");
    }

    #[test]
    fn task_list_empty_xml_renders_explicit_empty_state() {
        let mut t = ok_tool("task_list", "", 12);
        t.output_summary = Some("<background_tasks count=\"0\" />".to_string());
        let out = render(&t, 100, 4);
        assert!(out.contains("List background tasks · 12ms"), "{out}");
        assert!(out.contains("No background tasks."), "{out}");
        assert!(!out.contains("<background_tasks"), "{out}");
    }

    #[test]
    fn complete_failed_tool_treats_blank_output_as_missing_details() {
        let mut t = ToolCell::new_running("read_file", "Reading: src/main.rs");
        t.complete(
            "failed",
            19,
            String::new(),
            Some(" \n ".into()),
            Some("\t".into()),
        );
        let summary = t.output_summary.as_deref().unwrap();
        assert!(
            summary.contains("Read ended without a result payload: Reading: src/main.rs"),
            "{summary}"
        );
        assert!(
            summary.contains("outcome could not be confirmed"),
            "{summary}"
        );
        assert_eq!(t.output.as_deref(), t.output_summary.as_deref());
    }

    #[test]
    fn seconds_formatting_kicks_in_above_1s() {
        let t = ok_tool("build", "cargo build", 2500);
        let out = render(&t, 80, 2);
        assert!(out.contains("· 2.5s"), "sub-second boundary wrong: {out}");
    }

    #[test]
    fn output_summary_truncates_at_5_lines_with_marker() {
        let mut t = ok_tool("ls", "ls -la", 8);
        t.output_summary = Some(
            (1..=8)
                .map(|i| format!("file-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let out = render(&t, 80, 8);
        assert!(out.contains("file-1"));
        assert!(out.contains("file-5"));
        assert!(!out.contains("file-6"), "row 6 should have been trimmed");
        assert!(out.contains("… +3 lines"));
        assert!(out.contains("(Ctrl+O to view transcript)"));
    }

    #[test]
    fn render_strips_unsafe_terminal_control_bytes_from_tool_text() {
        let mut t = ok_tool("bash\x1b[31m", "printf '\x1b[31mboom\r\tok'", 8);
        t.output_summary = Some("line-1\x1b[2J\nline-2\u{009b}1m".into());

        let out = render(&t, 100, 6);
        assert!(out.contains("Bash"));
        let boom = out
            .find("boom")
            .expect("sanitized command keeps visible stdout");
        let ok = out
            .find("ok")
            .expect("sanitized command keeps text after tab");
        assert!(
            boom < ok,
            "sanitized command should preserve visible text order: {out}"
        );
        assert!(out.contains("line-1"));
        assert!(out.contains("line-2"));
        assert!(!out.contains("[2J"));
        assert!(!out.contains("[31m"));
        assert!(!out.contains("1m"));
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\r'));
        assert!(!out.contains('\u{9b}'));
    }

    #[test]
    fn ansi_wrapped_diff_summary_still_uses_diff_renderer() {
        let mut t = ok_tool("write_file", "/tmp/hello.py", 7);
        t.output_summary = Some(
            "\x1b[2mhello.py\x1b[0m\n\x1b[38;5;10m+\x1b[39m\x1b[38;5;10m#!/usr/bin/env python3\x1b[0m\n\x1b[38;5;10m+\x1b[39mprint(\"hello\")".into(),
        );

        let out = render(&t, 100, 8);
        assert!(out.contains("hello.py"));
        assert!(out.contains("   1 + #!/usr/bin/env python3"), "{out}");
        assert!(out.contains("   2 + print(\"hello\")"), "{out}");
        assert!(!out.contains("[38;5;10m"));
        assert!(!out.contains("[2m"));
    }

    #[test]
    fn raw_diff_preview_preserves_folded_change_count() {
        let mut t = ok_tool("str_replace", "src/hello.py", 7);
        t.output_summary = Some(
            "\
--- a/src/hello.py\n\
+++ b/src/hello.py\n\
@@ -1,2 +1,7 @@\n\
-print(\"old\")\n\
+print(\"new1\")\n\
+print(\"new2\")\n\
+print(\"new3\")\n\
+print(\"new4\")\n\
+print(\"new5\")\n\
… +1 more changed lines"
                .into(),
        );

        let out = render(&t, 100, 12);
        assert!(out.contains("   1 - print(\"old\")"), "{out}");
        assert!(out.contains("   1 + print(\"new1\")"), "{out}");
        assert!(out.contains("   5 + print(\"new5\")"), "{out}");
        assert!(out.contains("… +1 more changed lines"), "{out}");
        assert!(out.contains("(Ctrl+O to view transcript)"), "{out}");
    }

    #[test]
    fn diff_preview_prefix_uses_same_background_as_changed_line() {
        let mut t = ok_tool("str_replace", "src/main.rs", 120);
        t.output_summary = Some("@@ -1,1 +1,1 @@\n-fn old_name() {}\n+fn new_name() {}".into());

        let lines = t.display_lines(80);
        let diff_line = lines
            .iter()
            .find(|line| {
                let rendered: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                rendered.contains("old_name")
            })
            .expect("expected diff line");

        let prefix_bg = diff_line.spans[0].style.bg;
        let number_bg = diff_line.spans[1].style.bg;
        assert_eq!(prefix_bg, number_bg);
        assert!(prefix_bg.is_some());
    }

    #[test]
    fn placeholder_capture_summary_prefers_real_output_preview() {
        let mut t = ok_tool("bash", "$ head -300 /tmp/review.code.diff", 54);
        t.output_summary = Some("286 lines captured".into());
        t.output = Some("line 1\nline 2\nline 3\nline 4\nline 5\nline 6".into());
        let out = render(&t, 80, 9);
        assert!(out.contains("line 1"), "{out}");
        assert!(out.contains("line 5"), "{out}");
        assert!(!out.contains("286 lines captured"), "{out}");
        assert!(out.contains("(Ctrl+O to view transcript)"), "{out}");
    }

    #[test]
    fn long_plain_output_wraps_with_hanging_indent() {
        let mut t = ok_tool("bash", "$ head -300 /tmp/review.code.diff", 54);
        t.output = Some("crates/astra-cli/src/tui/bottom_pane/snapshots/astra__tui__bottom_pane__queue_preview_tests__bottom_surface_active_42.snap:6: trailing whitespace.".into());
        let out = render(&t, 56, 6);
        let rows: Vec<&str> = out.lines().filter(|line| !line.trim().is_empty()).collect();
        assert!(rows.len() >= 3, "{out}");
        assert!(
            rows.iter().skip(3).any(|row| row.starts_with("  │ ")),
            "wrapped tool output should keep a hanging indent: {rows:?}"
        );
    }

    #[test]
    fn long_diff_rows_wrap_without_losing_diff_indent() {
        let mut t = ok_tool("str_replace", "src/main.rs", 120);
        t.output_summary = Some(
            "@@ -1,1 +1,1 @@\n-fn old_name_with_a_very_long_signature(argument_one: usize, argument_two: usize) {}\n+fn new_name_with_a_very_long_signature(argument_one: usize, argument_two: usize) {}"
                .into(),
        );
        let out = render(&t, 72, 8);
        let rows: Vec<&str> = out.lines().filter(|line| !line.trim().is_empty()).collect();
        assert!(rows.len() >= 4, "{out}");
        assert!(
            rows.iter()
                .skip(3)
                .any(|row| row.starts_with("           ")),
            "wrapped diff continuation should stay indented under the diff gutter: {rows:?}"
        );
    }

    #[test]
    fn edit_tools_render_as_edited_cards_with_counts() {
        let mut t = ok_tool("write_file", "src/main.rs", 120);
        t.output = Some(
            r#"{"success":true,"_cli_unified_diff":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1,2 @@\n-fn old_name() {}\n+fn new_name() {}\n+fn helper() {}\n"}"#
                .into(),
        );

        let out = render(&t, 100, 10);
        assert!(out.contains("● Edited src/main.rs · +2 -1"), "{out}");
        assert!(!out.contains("Ran Write file"), "{out}");
        assert!(out.contains("   1 - fn old_name() {}"), "{out}");
        assert!(out.contains("   2 + fn helper() {}"), "{out}");
    }

    #[test]
    fn diff_check_style_output_stays_plain_text() {
        let mut t = ok_tool("bash", "git diff --check", 173);
        t.output = Some(
            "crates/astra-cli/src/tui/bottom_pane/snapshots/astra__tui__bottom_pane__queue_preview_tests__bottom_surface_active_42.snap:6: trailing whitespace.\n+\ncrates/astra-cli/src/tui/bottom_pane/snapshots/astra__tui__bottom_pane__queue_preview_tests__bottom_surface_active_42.snap:8: trailing whitespace.\n+"
                .into(),
        );

        let lines = t.display_lines(56);
        assert!(lines.iter().any(|line| {
            let rendered: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            rendered.starts_with("  ├ ")
        }));
        assert!(lines.iter().any(|line| {
            let rendered: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            rendered.contains("espace.")
        }));
        assert!(
            !lines.iter().any(|line| {
                let rendered: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                rendered.contains("   1 + ")
            }),
            "diff-check output should not be rendered as a unified diff"
        );
    }

    #[test]
    fn diff_rows_carry_full_surface_without_viewport_padding() {
        let mut t = ok_tool("str_replace", "src/main.rs", 120);
        t.output_summary = Some("@@ -1,1 +1,1 @@\n-fn old_name() {}\n+fn new_name() {}".into());

        let lines = t.display_lines(80);
        let diff_line = lines
            .iter()
            .find(|line| {
                let rendered: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                rendered.contains("new_name")
            })
            .expect("expected added diff line");

        assert_eq!(diff_line.style.bg, diff_line.spans[1].style.bg);
        assert!(
            diff_line.width() < 80,
            "full-row background is semantic metadata; spaces must not encode viewport width: {:?}",
            diff_line.spans
        );
    }

    #[test]
    fn diff_rows_paint_the_entire_rendered_terminal_row() {
        let mut t = ok_tool("str_replace", "src/main.rs", 120);
        t.output_summary = Some("@@ -1,1 +1,1 @@\n-fn old_name() {}\n+fn new_name() {}".into());

        let width = 80;
        let lines = sanitize_lines_for_terminal(t.display_lines(width));
        let paragraph = crate::tui::render::line_utils::FullRowParagraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false });
        let buffer = draw_widget(paragraph, width, 12);
        let added_row = (0..12)
            .find(|&y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("new_name")
            })
            .expect("rendered added diff row");
        let expected_bg = crate::tui::theme::current()
            .diff_add_style()
            .bg
            .expect("diff add style always defines a row background");

        assert!(
            (0..width).all(|x| buffer[(x, added_row)].bg == expected_bg),
            "an added edit row must retain its semantic background through the terminal edge"
        );
    }

    #[test]
    fn bash_unified_diff_uses_the_same_full_row_surface_as_edit() {
        let mut t = ok_tool("bash", "$ git diff -- pkg/txn.go", 42);
        t.output = Some("@@ -40,0 +40,2 @@\n+func sealAndRunWhenDrained() {}\n+\n".into());

        let width = 96;
        let lines = sanitize_lines_for_terminal(t.display_lines(width));
        let paragraph = crate::tui::render::line_utils::FullRowParagraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false });
        let buffer = draw_widget(paragraph, width, 10);
        let added_row = (0..10)
            .find(|&y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("sealAndRunWhenDrained")
            })
            .expect("rendered added bash diff row");
        let expected_bg = crate::tui::theme::current()
            .diff_add_style()
            .bg
            .expect("diff add style always defines a row background");
        assert!(
            (0..width).all(|x| buffer[(x, added_row)].bg == expected_bg),
            "a bash diff row must use the same full-width semantic surface as edit"
        );
    }

    #[test]
    fn blank_changed_lines_are_single_semantic_rows_not_visual_spacers() {
        let mut t = ok_tool("bash", "$ git diff -- src/main.rs", 42);
        t.output =
            Some("@@ -10,0 +10,3 @@\n+fn before_blank() {}\n+\n+fn after_blank() {}\n".into());

        let width = 72;
        let lines = sanitize_lines_for_terminal(t.display_lines(width));
        let rendered_line = |line: &ratatui::text::Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let before = lines
            .iter()
            .position(|line| rendered_line(line).contains("before_blank"))
            .expect("first changed line");
        let after = lines
            .iter()
            .position(|line| rendered_line(line).contains("after_blank"))
            .expect("last changed line");
        assert_eq!(after, before + 2, "blank source line must occupy one row");
        let blank = &lines[before + 1];
        assert!(
            rendered_line(blank).contains("+ "),
            "blank source line must retain its diff identity: {blank:?}"
        );
        assert_eq!(blank.style.bg, lines[before].style.bg);
        assert_eq!(blank.style.bg, lines[after].style.bg);

        let paragraph =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        let buffer = draw_widget(paragraph, width, 10);
        let find_row = |needle: &str| {
            (0..10)
                .find(|&y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .expect("changed line is visible")
        };
        let before_row = find_row("before_blank");
        let after_row = find_row("after_blank");
        assert_eq!(
            after_row,
            before_row + 2,
            "the blank added source line must retain the edit surface without inserting a spacer"
        );
    }

    #[test]
    fn wrapped_diff_rows_keep_gutter_structure_and_background() {
        let mut t = ok_tool("str_replace", "src/main.rs", 120);
        t.output_summary = Some(
            "@@ -1 +1 @@\n-println!(\"old\");\n+println!(\"this_is_a_very_long_replacement_line_without_spaces_to_force_wrapping\");".into(),
        );

        let lines = t.display_lines(48);
        let first_idx = lines
            .iter()
            .position(|line| {
                line.spans.len() >= 3
                    && line.spans[2].content.as_ref() == "+ "
                    && line
                        .spans
                        .iter()
                        .skip(3)
                        .any(|span| span.content.as_ref().contains("println!("))
            })
            .expect("expected first added diff row");
        let continuation = &lines[first_idx + 1];
        assert_eq!(continuation.spans[0].content.as_ref(), "    ");
        assert_eq!(continuation.spans[1].content.as_ref(), "     ");
        assert_eq!(continuation.spans[2].content.as_ref(), "  ");
        let bg = continuation.spans[1].style.bg;
        assert!(bg.is_some(), "expected wrapped diff background");
        assert_eq!(continuation.spans[0].style.bg, bg);
        assert_eq!(continuation.spans[2].style.bg, bg);
    }

    // ── Progress signals ─────────────────────────────────────────

    #[test]
    fn set_progress_is_monotonic() {
        let mut t = ToolCell::new_running("bash", "long");
        t.set_progress(10, 512);
        t.set_progress(25, 2_048);
        assert_eq!(t.progress_lines, 25);
        assert_eq!(t.progress_bytes, 2_048);

        // A regression (out-of-order or clobbering update) must not
        // roll counters back.
        t.set_progress(5, 100);
        assert_eq!(t.progress_lines, 25);
        assert_eq!(t.progress_bytes, 2_048);
    }

    #[test]
    fn long_running_bash_row_advertises_ctrl_b_when_enabled() {
        let mut t = ToolCell::new_running("bash", "$ cargo test");
        t.set_ctrl_b_background_hint(true);
        t.started_at = Instant::now() - std::time::Duration::from_secs(4);
        let out = render(&t, 100, 4);
        assert!(out.contains("Ctrl+B to background"), "{out}");
    }

    #[test]
    fn long_running_bash_row_does_not_advertise_ctrl_b_by_default() {
        let mut t = ToolCell::new_running("bash", "$ cargo test");
        t.started_at = Instant::now() - std::time::Duration::from_secs(4);
        let out = render(&t, 100, 4);
        assert!(!out.contains("Ctrl+B"), "{out}");
    }

    #[test]
    fn long_running_non_shell_row_does_not_advertise_ctrl_b() {
        let mut t = ToolCell::new_running("read_file", "Reading: src/main.rs");
        t.set_ctrl_b_background_hint(true);
        t.started_at = Instant::now() - std::time::Duration::from_secs(4);
        let out = render(&t, 100, 4);
        assert!(!out.contains("Ctrl+B"), "{out}");
    }

    #[test]
    fn format_bytes_picks_compact_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2_048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    // ── Snapshots ────────────────────────────────────────────────

    #[test]
    fn snapshot_ok_no_output_80() {
        crate::tui::testing::assert_tui_snapshot!(
            "tool_ok_no_output_80",
            render(&ok_tool("bash", "ls /tmp", 42), 80, 3)
        );
    }

    #[test]
    fn snapshot_ok_with_summary_80() {
        let mut t = ok_tool("read", "Cargo.toml", 120);
        t.output_summary = Some("[package]\nname = \"demo\"".into());
        crate::tui::testing::assert_tui_snapshot!("tool_ok_with_summary_80", render(&t, 80, 5));
    }

    #[test]
    fn snapshot_err_80() {
        crate::tui::testing::assert_tui_snapshot!(
            "tool_err_80",
            render(&err_tool("bash", "false", 10), 80, 3)
        );
    }
}
