//! Fanout group header computation and rendering.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::types::{BackgroundTaskFanoutMembership, BackgroundTaskRow, BackgroundTaskStatus};
use crate::cli::effects::truncate_label;

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

pub(crate) fn fanout_header_line(header: &FanoutHeader, dim: Style) -> Line<'static> {
    let theme = crate::tui::theme::current();
    let mut parts = vec![format!("{} target", header.target_count)];
    if header.unobserved > 0 {
        parts.push(format!("{} not observed", header.unobserved));
    }
    if header.pending > 0 {
        parts.push(format!("{} pending", header.pending));
    }
    if header.running > 0 {
        parts.push(format!("{} running", header.running));
    }
    if header.needs_input > 0 {
        parts.push(format!("{} needs input", header.needs_input));
    }
    if header.stopping > 0 {
        parts.push(format!("{} stopping", header.stopping));
    }
    if header.done > 0 {
        parts.push(format!("{} completed", header.done));
    }
    if header.done_with_issues > 0 {
        parts.push(format!("{} completed with issues", header.done_with_issues));
    }
    if header.failed > 0 {
        parts.push(format!("{} failed", header.failed));
    }
    if header.interrupted > 0 {
        parts.push(format!("{} interrupted", header.interrupted));
    }
    if header.stopped > 0 {
        parts.push(format!("{} stopped", header.stopped));
    }
    if header.unavailable > 0 {
        parts.push(format!("{} unavailable", header.unavailable));
    }

    Line::from(vec![
        Span::styled("  ▣ ".to_string(), dim),
        Span::styled(
            truncate_label(&header.title, 30),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", parts.join(" · ")), dim),
    ])
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

    fn header_line_text(header: &FanoutHeader) -> String {
        fanout_header_line(header, Style::default())
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

        let text = header_line_text(&header);
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
        assert!(header_line_text(&header).contains("8 not observed"));
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
}
