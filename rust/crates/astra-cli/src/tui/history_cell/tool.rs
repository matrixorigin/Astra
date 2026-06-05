//! Tool-invocation history cell — the `• Ran bash (42ms)` block.
//!
//! Three visual states:
//! - **Running** — accent bullet, shimmer title, elapsed from
//!   construction `Instant`, optional Braille spinner+progress bar
//!   if the tool has been running more than 3 s. Not persisted
//!   until the final `complete()` call.
//! - **Success** — green bullet, `Ran <name> (Xms)` title, optional
//!   description (`│ <cmd>`) + output summary (`└ <first 5 lines>`).
//! - **Failed** — red bullet, `Failed <name>`, otherwise identical
//!   to Success.
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
use std::time::Instant;

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::HistoryCell;
use crate::cli::tool_result_status::tool_result_status_is_success;
use crate::tui::render::line_utils::sanitize_terminal_text;
use crate::tui::turn_event::{ToolStatus as PersistStatus, TurnEvent};

/// Live status. `Running` is intentionally separate from the
/// persisted `TurnEvent::Tool.status` enum — a still-running tool
/// never reaches disk, so the schema only carries terminal states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolStatus {
    Running,
    Success,
    Failed,
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
    /// Stamped on the live → terminal transition (either
    /// `complete()` or `finalize()`). Lets the active-slot gradient
    /// gutter pin its phase at the freeze moment.
    frozen_at: super::FreezeStamp,
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
            frozen_at: super::FreezeStamp::default(),
        }
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
        self.status = if tool_result_status_is_success(status_str) {
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
        self.frozen_at.stamp_now();
    }

    #[allow(dead_code)]
    pub fn with_ts(mut self, ts: impl Into<String>) -> Self {
        self.ts = Some(ts.into());
        self
    }

    /// Resume constructor. Duration is restored verbatim; the
    /// `started_at` Instant is meaningless on reload (we can't
    /// reconstruct a past wall clock) so we pin it to `now() -
    /// duration` so any `elapsed()` call still lines up with the
    /// persisted string.
    #[allow(dead_code)]
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
            PersistStatus::Failed => ToolStatus::Failed,
        };
        let started_at = Instant::now()
            .checked_sub(std::time::Duration::from_millis(duration_ms))
            .unwrap_or_else(Instant::now);
        Some(Self {
            name,
            description,
            status,
            started_at,
            duration_ms: Some(duration_ms),
            output_summary,
            output,
            ts,
            progress_lines: 0,
            progress_bytes: 0,
            // Resumed from persistence — already settled. See
            // `FreezeStamp::revived` for the launch-independent
            // phase rationale.
            frozen_at: super::FreezeStamp::revived(),
        })
    }

    fn bullet(&self) -> Span<'static> {
        match self.status {
            // Running uses the theme accent so the in-progress row
            // pops from dim scrollback — a dim bullet was
            // invisible on many terminals and users reported fast
            // tool calls felt like they skipped the "running"
            // phase entirely.
            ToolStatus::Running => {
                let theme = crate::tui::theme::current();
                Span::styled("• ", Style::default().fg(theme.accent).bold())
            }
            ToolStatus::Success => Span::styled("• ", Style::default().fg(Color::Green).bold()),
            ToolStatus::Failed => Span::styled("• ", Style::default().fg(Color::Red).bold()),
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

    fn title_text(&self) -> &'static str {
        match self.status {
            ToolStatus::Running => "Running",
            ToolStatus::Success => "Ran",
            ToolStatus::Failed => "Failed",
        }
    }

    /// Sub-line rendered under the header for tools that are still
    /// running past the 3 s grace window.
    ///
    /// Two shapes:
    /// - **Signal mode** (any `progress_lines` / `progress_bytes`
    ///   arrived) — a Braille spinner + `"streaming · N lines · K KB"`
    ///   counter. Honest, monotonic, no fake percentages.
    /// - **Indeterminate mode** (no progress signal ever arrived —
    ///   non-streaming tools like `read_file`, `git_log`, skill
    ///   dispatch) — a breathing bar with a small block sliding back
    ///   and forth. Purely time-based; makes "still working" visible
    ///   without pretending to track progress.
    fn progress_line(&self, width: usize, elapsed_ms: u64) -> Line<'static> {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame_idx = ((elapsed_ms / 80) % FRAMES.len() as u64) as usize;
        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(Color::DarkGray);
        let spinner = Span::styled(
            FRAMES[frame_idx].to_string(),
            Style::default().fg(theme.accent),
        );

        // Signal mode: show real counters when the tool actually
        // streamed something.
        if self.progress_lines > 0 || self.progress_bytes > 0 {
            let body = format!(
                " streaming · {} {} · {}",
                self.progress_lines,
                if self.progress_lines == 1 {
                    "line"
                } else {
                    "lines"
                },
                format_bytes(self.progress_bytes),
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

impl HistoryCell for ToolCell {
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

        // Spinner + progress bar for long-running tools.
        if self.status == ToolStatus::Running {
            let elapsed = self.started_at.elapsed().as_millis() as u64;
            if elapsed >= 3_000 {
                lines.push(self.progress_line(w, elapsed));
            }
        }

        // `│ <description>` — the command, path, or summary line.
        if !self.description.is_empty() {
            let max_w = w.saturating_sub(4);
            let description = sanitize_terminal_text(&self.description);
            for dl in description.lines().take(2) {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", dim),
                    Span::raw(truncate_by_width(dl, max_w)),
                ]));
            }
        }

        // `└ <output summary>` — diff renderer for +/- content,
        // plain truncated preview otherwise.
        if let Some(ref summary) = self.output_summary {
            let summary = sanitize_terminal_text(summary);
            let has_diff = summary
                .lines()
                .any(|l| l.starts_with('+') || l.starts_with('-'));
            if has_diff {
                let diff_lines = crate::tui::diff_render::render_diff_lines(&summary, 10);
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
        self.frozen_at.stamp_now();
    }

    fn frozen_phase(&self) -> Option<f32> {
        self.frozen_at.phase()
    }

    fn to_persist(&self) -> Option<TurnEvent> {
        // Running cells never hit disk — the journal records
        // committed turns only. `finalize()` / `complete()` must
        // run first.
        let status = match self.status {
            ToolStatus::Success => PersistStatus::Success,
            ToolStatus::Failed => PersistStatus::Failed,
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
        t.complete("success", 42, String::new(), Some("3 entries".into()), None);
        assert_eq!(t.status, ToolStatus::Success);
        assert_eq!(t.duration_ms, Some(42));
        assert_eq!(t.output_summary.as_deref(), Some("3 entries"));
    }

    #[test]
    fn complete_treats_ok_as_success() {
        let mut t = ToolCell::new_running("bash", "ls");
        t.complete("ok", 42, String::new(), Some("3 entries".into()), None);
        assert_eq!(t.status, ToolStatus::Success);
    }

    #[test]
    fn finalize_demotes_stuck_running_to_failed() {
        // If a turn aborts mid-tool, finalize should still produce
        // a persistable record rather than silently losing the row.
        let mut t = ToolCell::new_running("bash", "slow op");
        t.finalize();
        assert_eq!(t.status, ToolStatus::Failed);
        assert!(t.duration_ms.is_some(), "duration snapshotted on finalize");
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
    fn success_header_has_ran_prefix() {
        let t = ok_tool("bash", "ls /tmp", 42);
        let out = render(&t, 80, 3);
        assert!(out.contains("• Ran bash"), "unexpected header: {out}");
        assert!(out.contains("(42ms)"));
        assert!(out.contains("│ ls /tmp"));
    }

    #[test]
    fn failed_header_is_red_and_says_failed() {
        let t = err_tool("bash", "false", 10);
        let out = render(&t, 80, 3);
        assert!(out.contains("• Failed bash"));
    }

    #[test]
    fn seconds_formatting_kicks_in_above_1s() {
        let t = ok_tool("build", "cargo build", 2500);
        let out = render(&t, 80, 2);
        assert!(out.contains("(2.5s)"), "sub-second boundary wrong: {out}");
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
    }

    #[test]
    fn render_strips_unsafe_terminal_control_bytes_from_tool_text() {
        let mut t = ok_tool("bash\x1b[31m", "printf '\x1b[31mboom\r\tok'", 8);
        t.output_summary = Some("line-1\x1b[2J\nline-2\u{009b}1m".into());

        let out = render(&t, 100, 6);
        assert!(out.contains("bash"));
        assert!(out.contains("boom\tok"));
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
