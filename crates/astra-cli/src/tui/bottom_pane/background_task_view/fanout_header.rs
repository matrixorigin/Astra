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
    pub running: usize,
    pub stopping: usize,
    pub done: usize,
    pub failed: usize,
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
        running: 0,
        stopping: 0,
        done: 0,
        failed: 0,
        stopped: 0,
        unavailable: 0,
    };

    for row in member_indices.iter().filter_map(|idx| rows.get(*idx)) {
        match row.status {
            BackgroundTaskStatus::Pending
            | BackgroundTaskStatus::Running
            | BackgroundTaskStatus::WaitingForInput => header.running += 1,
            BackgroundTaskStatus::Stopping => header.stopping += 1,
            BackgroundTaskStatus::Completed | BackgroundTaskStatus::CompletedWithIssues => {
                header.done += 1
            }
            BackgroundTaskStatus::Interrupted | BackgroundTaskStatus::Failed => header.failed += 1,
            BackgroundTaskStatus::Cancelled => header.stopped += 1,
            BackgroundTaskStatus::Unavailable => header.unavailable += 1,
        }
    }

    header
}

pub(crate) fn fanout_header_line(header: &FanoutHeader, dim: Style) -> Line<'static> {
    let theme = crate::tui::theme::current();
    let mut parts = vec![format!("{} target", header.target_count)];
    if header.running > 0 {
        parts.push(format!("{} running", header.running));
    }
    if header.stopping > 0 {
        parts.push(format!("{} stopping", header.stopping));
    }
    if header.done > 0 {
        parts.push(format!("{} done", header.done));
    }
    if header.failed > 0 {
        parts.push(format!("{} failed", header.failed));
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
