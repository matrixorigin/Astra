//! Fanout group header computation and rendering.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::types::{BackgroundTaskFanoutMembership, BackgroundTaskRow, BackgroundTaskStatus};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FanoutHeader {
    pub title: String,
    pub target_count: usize,
    /// Number of expected slots that are not represented by an observed row.
    /// This is intentionally surfaced as "not observed" rather than guessed
    /// as pending or failed.
    pub unobserved: usize,
    pub pending: usize,
    pub running: usize,
    pub needs_input: usize,
    pub stopping: usize,
    pub done: usize,
    pub done_with_issues: usize,
    pub failed: usize,
    pub interrupted: usize,
    pub stopped: usize,
    pub unavailable: usize,
}

pub(crate) fn compute_fanout_header(
    fanout: &BackgroundTaskFanoutMembership,
    member_indices: &[usize],
    rows: &[BackgroundTaskRow],
) -> FanoutHeader {
    let mut header = FanoutHeader {
        title: if fanout.group_title.trim().is_empty() {
            fanout.group_id.clone()
        } else {
            fanout.group_title.clone()
        },
        target_count: fanout.target_count,
        unobserved: 0,
        pending: 0,
        running: 0,
        needs_input: 0,
        stopping: 0,
        done: 0,
        done_with_issues: 0,
        failed: 0,
        interrupted: 0,
        stopped: 0,
        unavailable: 0,
    };

    let mut observed_slots = std::collections::HashSet::new();
    for row in member_indices.iter().filter_map(|idx| rows.get(*idx)) {
        if let Some(membership) = row.fanout.as_ref() {
            observed_slots.insert(membership.slot_index);
        }
        match row.status {
            BackgroundTaskStatus::Pending => header.pending += 1,
            BackgroundTaskStatus::Running => header.running += 1,
            BackgroundTaskStatus::WaitingForInput => header.needs_input += 1,
            BackgroundTaskStatus::Stopping => header.stopping += 1,
            BackgroundTaskStatus::Completed => header.done += 1,
            BackgroundTaskStatus::CompletedWithIssues => header.done_with_issues += 1,
            BackgroundTaskStatus::Failed => header.failed += 1,
            BackgroundTaskStatus::Interrupted => header.interrupted += 1,
            BackgroundTaskStatus::Cancelled => header.stopped += 1,
            BackgroundTaskStatus::Unavailable => header.unavailable += 1,
        }
    }
    header.unobserved = fanout.target_count.saturating_sub(observed_slots.len());

    header
}

/// Render a fanout summary within a terminal width budget.
///
/// A fanout can carry many independent slot states. Rendering the title and
/// every state verbatim makes the useful part of the header disappear on a
/// narrow terminal because `Buffer::set_line` clips from the right. Reserve
/// space for the target count, prefer attention-worthy states, and only then
/// spend the remaining columns on ordinary progress states. The full set is
/// still rendered when the width allows it.
pub(crate) fn fanout_header_line(
    header: &FanoutHeader,
    dim: Style,
    available_width: usize,
) -> Line<'static> {
    let theme = crate::tui::theme::current();
    if available_width == 0 {
        return Line::default();
    }

    let prefix = "  ▣ ";
    let target = format!("{} target", header.target_count);
    let mut candidates = Vec::new();
    push_status(&mut candidates, header.needs_input, "needs input", 100);
    push_status(&mut candidates, header.failed, "failed", 95);
    push_status(
        &mut candidates,
        header.done_with_issues,
        "completed with issues",
        90,
    );
    push_status(&mut candidates, header.unavailable, "unavailable", 85);
    push_status(&mut candidates, header.interrupted, "interrupted", 80);
    push_status(&mut candidates, header.unobserved, "not observed", 75);
    push_status(&mut candidates, header.stopping, "stopping", 70);
    push_status(&mut candidates, header.running, "running", 40);
    push_status(&mut candidates, header.pending, "pending", 35);
    push_status(&mut candidates, header.done, "completed", 10);
    push_status(&mut candidates, header.stopped, "stopped", 5);

    // Keep a small title visible whenever possible. Optional states are
    // admitted by attention priority, with stable source order for ties.
    let prefix_width = UnicodeWidthStr::width(prefix);
    let target_width = UnicodeWidthStr::width(target.as_str());
    let separator_width = UnicodeWidthStr::width(" · ");
    let title_min_width = if available_width >= 72 {
        12
    } else if available_width >= 48 {
        8
    } else {
        0
    };
    let optional_budget = available_width
        .saturating_sub(prefix_width + target_width + separator_width + title_min_width);
    let mut ranked = candidates.iter().enumerate().collect::<Vec<_>>();
    ranked.sort_by(|(a_idx, a), (b_idx, b)| {
        b.priority.cmp(&a.priority).then_with(|| a_idx.cmp(b_idx))
    });
    let mut selected = vec![false; candidates.len()];
    let mut optional_used = 0usize;
    for (idx, candidate) in ranked {
        let rendered = format!("{} {}", candidate.count, candidate.text);
        let cost = separator_width + UnicodeWidthStr::width(rendered.as_str());
        if optional_used.saturating_add(cost) <= optional_budget {
            selected[idx] = true;
            optional_used += cost;
        }
    }

    let mut parts = vec![target];
    parts.extend(
        candidates
            .iter()
            .enumerate()
            .filter(|(idx, _)| selected[*idx])
            .map(|(_, candidate)| format!("{} {}", candidate.count, candidate.text)),
    );
    let status_text = parts.join(" · ");
    let title_budget = available_width.saturating_sub(
        prefix_width + separator_width + UnicodeWidthStr::width(status_text.as_str()),
    );
    let title = truncate_to_columns(&header.title, title_budget.min(30));

    let mut spans = vec![Span::styled(prefix.to_string(), dim)];
    if !title.is_empty() {
        spans.push(Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" · ".to_string(), dim));
    }
    spans.push(Span::styled(status_text, dim));
    Line::from(spans)
}

#[derive(Clone, Debug)]
struct StatusPart {
    count: usize,
    text: &'static str,
    priority: u8,
}

fn push_status(parts: &mut Vec<StatusPart>, count: usize, text: &'static str, priority: u8) {
    if count > 0 {
        parts.push(StatusPart {
            count,
            text,
            priority,
        });
    }
}

fn truncate_to_columns(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let budget = max_width - 1;
    let mut result = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width.saturating_add(char_width) > budget {
            break;
        }
        width += char_width;
        result.push(ch);
    }
    result.push('…');
    result
}

pub(crate) fn fanout_slot_title(row: &BackgroundTaskRow) -> String {
    let Some(fanout) = row.fanout.as_ref() else {
        return row.title.clone();
    };
    let label = if fanout.slot_label.trim().is_empty() {
        row.title.as_str()
    } else {
        fanout.slot_label.as_str()
    };
    format!("slot {}: {}", fanout.slot_index + 1, label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::bottom_pane::background_task_view::types::BackgroundTaskRow;

    fn membership(
        group_id: &str,
        target_count: usize,
        slot_index: usize,
    ) -> BackgroundTaskMembership {
        BackgroundTaskMembership {
            group_id: group_id.to_string(),
            group_title: "review fanout".to_string(),
            target_count,
            slot_index,
            slot_label: format!("slot {slot_index}"),
        }
    }

    // Alias keeps the fixtures readable while the production type remains the
    // shared background-task membership contract.
    type BackgroundTaskMembership = BackgroundTaskFanoutMembership;

    fn row(status: &str, slot_index: usize) -> BackgroundTaskRow {
        BackgroundTaskRow::shell(
            format!("slot-{slot_index}"),
            status,
            1,
            format!("slot {slot_index}"),
            None,
            None,
            None,
        )
        .with_fanout(membership("review-1", 10, slot_index))
    }

    fn header_line_text(header: &FanoutHeader, width: usize) -> String {
        fanout_header_line(header, Style::default(), width)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn fanout_summary_preserves_actionable_statuses_and_terminal_causes() {
        let rows = vec![
            row("pending", 0),
            row("running", 1),
            row("waiting_for_input", 2),
            row("stopping", 3),
            row("completed", 4),
            row("completed_with_issues", 5),
            row("failed", 6),
            row("interrupted", 7),
            row("cancelled", 8),
            row("unavailable", 9),
        ];
        let fanout = rows[0].fanout.as_ref().unwrap().clone();
        let indices: Vec<_> = (0..rows.len()).collect();
        let header = compute_fanout_header(&fanout, &indices, &rows);

        assert_eq!(header.target_count, 10);
        assert_eq!(header.unobserved, 0);
        assert_eq!(header.pending, 1);
        assert_eq!(header.running, 1);
        assert_eq!(header.needs_input, 1);
        assert_eq!(header.stopping, 1);
        assert_eq!(header.done, 1);
        assert_eq!(header.done_with_issues, 1);
        assert_eq!(header.failed, 1);
        assert_eq!(header.interrupted, 1);
        assert_eq!(header.stopped, 1);
        assert_eq!(header.unavailable, 1);

        let text = header_line_text(&header, 220);
        for label in [
            "1 pending",
            "1 running",
            "1 needs input",
            "1 completed",
            "1 completed with issues",
            "1 failed",
            "1 interrupted",
            "1 stopped",
            "1 unavailable",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
    }

    #[test]
    fn fanout_summary_reports_unobserved_slots_without_inventing_status() {
        let rows = vec![row("running", 0), row("completed", 3)];
        let fanout = rows[0].fanout.as_ref().unwrap().clone();
        let indices = vec![0, 1];
        let header = compute_fanout_header(&fanout, &indices, &rows);

        assert_eq!(header.target_count, 10);
        assert_eq!(header.unobserved, 8);
        assert_eq!(header.pending, 0);
        assert_eq!(header.failed, 0);
        assert!(header_line_text(&header, 120).contains("8 not observed"));
    }

    #[test]
    fn fanout_summary_does_not_underflow_when_observed_rows_exceed_target() {
        let rows = vec![row("completed", 0), row("completed", 1)];
        let mut fanout = rows[0].fanout.as_ref().unwrap().clone();
        fanout.target_count = 1;
        let header = compute_fanout_header(&fanout, &[0, 1], &rows);

        assert_eq!(header.unobserved, 0);
        assert_eq!(header.done, 2);
    }

    #[test]
    fn fanout_summary_counts_duplicate_slot_rows_once_for_observation() {
        let rows = vec![row("running", 0), row("completed", 0)];
        let fanout = rows[0].fanout.as_ref().unwrap().clone();
        let header = compute_fanout_header(&fanout, &[0, 1], &rows);

        assert_eq!(header.unobserved, 9);
        assert_eq!(header.running, 1);
        assert_eq!(header.done, 1);
    }

    #[test]
    fn narrow_header_prioritizes_actionable_states_over_a_long_title() {
        let mut rows = vec![
            row("waiting_for_input", 0),
            row("failed", 1),
            row("unavailable", 2),
        ];
        for row in &mut rows {
            let fanout = row.fanout.as_mut().unwrap();
            fanout.target_count = 3;
            fanout.group_title =
                "A very long fanout title that should yield to action states".to_string();
        }
        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 4));
        super::super::list_render::render_list(
            &rows,
            0,
            ratatui::layout::Rect::new(0, 0, 80, 4),
            &mut buffer,
        );
        let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
        let line = rendered.lines().nth(1).expect("fanout header row");
        assert!(line.contains("1 needs input"), "{rendered}");
        assert!(line.contains("1 failed"), "{rendered}");
        assert!(line.contains("1 unavailable"), "{rendered}");
        assert!(
            !line.contains("A very long fanout title that should yield"),
            "long group titles must yield to actionable state labels: {rendered}"
        );
        assert!(
            UnicodeWidthStr::width(line) <= 80,
            "header must fit the terminal: {line:?}"
        );
    }
}
