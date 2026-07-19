//! List-mode rendering: entry computation, row layout, and paint.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::fanout_header::{
    FanoutHeader, compute_fanout_header, fanout_header_line, fanout_slot_title,
};
use super::types::{
    BackgroundTaskRow, BackgroundTaskStatus, BackgroundTaskStatusExt, format_elapsed,
    pluralize_with_count,
};
use crate::cli::effects::truncate_label;

pub(crate) enum BackgroundTaskListEntry<'a> {
    FanoutHeader(FanoutHeader),
    Row {
        row_idx: usize,
        row: &'a BackgroundTaskRow,
        grouped: bool,
    },
}

impl BackgroundTaskListEntry<'_> {
    pub(crate) fn row_index(&self) -> Option<usize> {
        match self {
            Self::FanoutHeader(_) => None,
            Self::Row { row_idx, .. } => Some(*row_idx),
        }
    }
}

pub(crate) fn background_task_list_entries(
    rows: &[BackgroundTaskRow],
) -> Vec<BackgroundTaskListEntry<'_>> {
    let mut entries = Vec::with_capacity(rows.len());
    let mut rendered = vec![false; rows.len()];

    for idx in 0..rows.len() {
        if rendered[idx] {
            continue;
        }

        let Some(fanout) = rows[idx].fanout.as_ref() else {
            rendered[idx] = true;
            entries.push(BackgroundTaskListEntry::Row {
                row_idx: idx,
                row: &rows[idx],
                grouped: false,
            });
            continue;
        };

        let member_indices = rows
            .iter()
            .enumerate()
            .filter_map(|(member_idx, row)| {
                row.fanout
                    .as_ref()
                    .is_some_and(|member| member.group_id == fanout.group_id)
                    .then_some(member_idx)
            })
            .collect::<Vec<_>>();
        entries.push(BackgroundTaskListEntry::FanoutHeader(
            compute_fanout_header(fanout, &member_indices, rows),
        ));
        for member_idx in member_indices {
            rendered[member_idx] = true;
            entries.push(BackgroundTaskListEntry::Row {
                row_idx: member_idx,
                row: &rows[member_idx],
                grouped: true,
            });
        }
    }

    entries
}

pub(crate) fn render_list(
    rows: &[BackgroundTaskRow],
    selected: usize,
    area: Rect,
    buf: &mut Buffer,
) {
    let theme = crate::tui::theme::current();
    let dim = Style::default().fg(theme.dim);
    let title_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let running = rows
        .iter()
        .filter(|row| row.status == BackgroundTaskStatus::Running)
        .count();
    let waiting = rows
        .iter()
        .filter(|row| row.status == BackgroundTaskStatus::WaitingForInput)
        .count();
    let stopping = rows
        .iter()
        .filter(|row| row.status == BackgroundTaskStatus::Stopping)
        .count();
    let failed = rows
        .iter()
        .filter(|row| row.status == BackgroundTaskStatus::Failed)
        .count();
    let interrupted = rows
        .iter()
        .filter(|row| row.status == BackgroundTaskStatus::Interrupted)
        .count();
    let quiet = rows
        .iter()
        .filter(|row| row.no_recent_output_ms.is_some())
        .count();
    let header = if rows.is_empty() {
        "  Tasks".to_string()
    } else {
        let mut parts = vec![format!("{} total", rows.len())];
        if running > 0 {
            parts.push(format!("{running} running"));
        }
        if waiting > 0 {
            parts.push(pluralize_with_count(waiting, "needs input", "need input"));
        }
        if stopping > 0 {
            parts.push(format!("{stopping} stopping"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if interrupted > 0 {
            parts.push(format!("{interrupted} interrupted"));
        }
        if quiet > 0 {
            parts.push(format!("{quiet} quiet"));
        }
        format!("  Tasks · {}", parts.join(" · "))
    };
    buf.set_line(
        area.x,
        area.y,
        &Line::from(Span::styled(header, title_style)),
        area.width,
    );

    if rows.is_empty() {
        if area.height >= 2 {
            buf.set_line(
                area.x,
                area.y + 1,
                &Line::from(Span::styled("  No tasks.", dim)),
                area.width,
            );
        }
        return;
    }

    let body_y = area.y + 1;
    let body_h = area.height.saturating_sub(1) as usize;
    let entries = background_task_list_entries(rows);
    let selected_entry = entries
        .iter()
        .position(|entry| entry.row_index() == Some(selected))
        .unwrap_or(0);
    let window_start = selected_entry.saturating_add(1).saturating_sub(body_h);
    let mut visible_row_number = entries
        .iter()
        .take(window_start)
        .filter(|entry| entry.row_index().is_some())
        .count();
    for (i, entry) in entries.iter().skip(window_start).take(body_h).enumerate() {
        let line = match entry {
            BackgroundTaskListEntry::FanoutHeader(header) => fanout_header_line(header, dim),
            BackgroundTaskListEntry::Row {
                row_idx,
                row,
                grouped,
            } => {
                let is_selected = *row_idx == selected;
                let marker_style = if is_selected {
                    Style::default()
                        .fg(theme.selected_fg)
                        .bg(theme.selected_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    dim
                };
                let row_style = if is_selected {
                    Style::default()
                        .fg(row.status.color())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(row.status.color())
                };
                visible_row_number += 1;
                let marker = if is_selected { "› " } else { "  " };
                let index = format!("{}. ", visible_row_number);
                let activity = row
                    .no_recent_output_ms
                    .map(|ms| format!("  quiet {}", format_elapsed(ms)))
                    .unwrap_or_default();
                let control = row
                    .live_control
                    .list_label()
                    .map(|label| format!("  {label}"))
                    .unwrap_or_default();
                let ownership = if row.kind == super::types::BackgroundTaskKind::LocalAgent
                    && !row.run_in_background
                {
                    "  foreground"
                } else {
                    ""
                };
                let meta = format!(
                    "{}  {}  {}{}{}{}  ",
                    row.kind.as_str(),
                    row.status.label(),
                    format_elapsed(row.elapsed_ms),
                    activity,
                    control,
                    ownership,
                );
                let title = if *grouped {
                    fanout_slot_title(row)
                } else {
                    row.title.clone()
                };
                let (id, title) = compact_list_row_text(
                    row,
                    title.as_str(),
                    area.width as usize,
                    marker,
                    &index,
                    &meta,
                );
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(index, row_style),
                    Span::styled(id, row_style),
                    Span::styled("  ", dim),
                    Span::styled(meta, dim),
                    Span::styled(title, row_style),
                ])
            }
        };
        buf.set_line(area.x, body_y + i as u16, &line, area.width);
    }
}

fn compact_list_row_text(
    row: &BackgroundTaskRow,
    title: &str,
    width: usize,
    marker: &str,
    index: &str,
    meta: &str,
) -> (String, String) {
    let min_title = if width >= 44 {
        10
    } else if width >= 34 {
        6
    } else {
        0
    };
    let fixed_without_id_or_title =
        marker.chars().count() + index.chars().count() + 2 + meta.chars().count();
    let max_id = if width >= 72 {
        24
    } else if width >= 44 {
        14
    } else {
        9
    };
    let id_budget = width
        .saturating_sub(fixed_without_id_or_title + min_title)
        .min(max_id);
    let id = truncate_label(&row.id, id_budget);
    let used = fixed_without_id_or_title + id.chars().count();
    let title_budget = width.saturating_sub(used);
    let title = truncate_label(title, title_budget);
    (id, title)
}
