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
use unicode_width::UnicodeWidthStr;

use super::types::{
    BackgroundTaskRow, BackgroundTaskStatus, BackgroundTaskStatusExt, LiveControlState,
    format_elapsed, pluralize_with_count,
};

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
    // Build the fanout membership index once per projection. The old path
    // scanned every row again for each first-seen group, which made a list
    // with many groups approach O(N×G) work on every repaint and navigation
    // key. Borrowed group ids keep this temporary index allocation small and
    // do not create another source of lifecycle state.
    let mut fanout_members = std::collections::HashMap::<&str, Vec<usize>>::new();
    for (idx, row) in rows.iter().enumerate() {
        if let Some(fanout) = row.fanout.as_ref() {
            fanout_members
                .entry(fanout.group_id.as_str())
                .or_default()
                .push(idx);
        }
    }
    let mut rendered_groups = std::collections::HashSet::<&str>::new();

    for idx in 0..rows.len() {
        let Some(fanout) = rows[idx].fanout.as_ref() else {
            entries.push(BackgroundTaskListEntry::Row {
                row_idx: idx,
                row: &rows[idx],
                grouped: false,
            });
            continue;
        };

        let group_id = fanout.group_id.as_str();
        if !rendered_groups.insert(group_id) {
            continue;
        }
        let member_indices = fanout_members.get(group_id).cloned().unwrap_or_default();
        entries.push(BackgroundTaskListEntry::FanoutHeader(
            compute_fanout_header(fanout, &member_indices, rows),
        ));
        for member_idx in member_indices {
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
    let title_style = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
    let (mut running, mut waiting, mut stopping, mut failed, mut interrupted, mut stale) =
        (0, 0, 0, 0, 0, 0);
    for row in rows {
        if row.live_control == LiveControlState::StaleHandle {
            stale += 1;
            continue;
        }
        match row.status {
            BackgroundTaskStatus::Running => running += 1,
            BackgroundTaskStatus::WaitingForInput => waiting += 1,
            BackgroundTaskStatus::Stopping => stopping += 1,
            BackgroundTaskStatus::Failed => failed += 1,
            BackgroundTaskStatus::Interrupted => interrupted += 1,
            _ => {}
        }
    }
    let mut counts = String::new();
    if waiting > 0 {
        counts.push_str(" · ");
        counts.push_str(&pluralize_with_count(waiting, "needs input", "need input"));
    }
    for (count, status) in [
        (failed, "failed"),
        (stopping, "stopping"),
        (stale, "stale"),
        (interrupted, "interrupted"),
        (running, "running"),
    ] {
        if count > 0 {
            counts.push_str(&format!(" · {count} {status}"));
        }
    }
    buf.set_line(
        area.x,
        area.y,
        &Line::from(vec![
            Span::styled("  Background tasks", title_style),
            Span::styled(counts, dim),
        ]),
        area.width,
    );

    if rows.is_empty() {
        if area.height >= 2 {
            buf.set_line(
                area.x,
                area.y + 1,
                &Line::from(Span::styled("  No background tasks for this session.", dim)),
                area.width,
            );
        }
        return;
    }

    const STATE_WIDTH: usize = 24;
    const TIME_WIDTH: usize = 8;
    let width = usize::from(area.width);
    let columns = width >= 66 && area.height >= 4;
    let name_width = width.saturating_sub(STATE_WIDTH + TIME_WIDTH + 8);
    let body_y = area.y + 1 + u16::from(columns);
    let body_h = usize::from(area.bottom().saturating_sub(body_y));
    if columns {
        let header = format!(
            "  {:<name_width$}  {:<STATE_WIDTH$}  {:>TIME_WIDTH$}",
            "Task", "State", "Time",
        );
        buf.set_line(area.x, area.y + 1, &Line::styled(header, dim), area.width);
    }
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
            BackgroundTaskListEntry::FanoutHeader(header) => {
                fanout_header_line(header, dim, usize::from(area.width))
            }
            BackgroundTaskListEntry::Row {
                row_idx,
                row,
                grouped,
            } => {
                let is_selected = *row_idx == selected;
                let marker = if is_selected { "› " } else { "  " };
                let row_style = if is_selected {
                    Style::default()
                        .fg(theme.selected_fg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg)
                };
                let status_style = Style::default().fg(row.status.color());
                visible_row_number += 1;
                let title = if *grouped {
                    fanout_slot_title(row)
                } else if row.title.trim().is_empty() {
                    row.id.clone()
                } else {
                    row.title.clone()
                };
                let label = format!("{visible_row_number}. {title}");
                let status = if row.live_control == LiveControlState::StaleHandle {
                    format!("stale · {}", row.status.label())
                } else if let Some(code) = row.exit_code {
                    format!("{} · exit {code}", row.status.label())
                } else {
                    row.status.label().to_string()
                };
                let elapsed = format_elapsed(row.elapsed_ms);
                let truncate = crate::tui::history_cell::truncate_by_width;
                let mut spans = vec![Span::styled(marker, Style::default().fg(theme.accent))];
                if columns {
                    let label = truncate(&label, name_width);
                    let remaining = name_width.saturating_sub(label.width());
                    let kind = format!(" · {}", row.kind.as_str());
                    let kind = if kind.width() <= remaining {
                        kind.as_str()
                    } else {
                        ""
                    };
                    let padding = " ".repeat(remaining.saturating_sub(kind.width()));
                    let status = truncate(&status, STATE_WIDTH);
                    let status_pad = " ".repeat(STATE_WIDTH.saturating_sub(status.width()));
                    spans.extend([
                        Span::styled(label, row_style),
                        Span::styled(format!("{kind}{padding}"), dim),
                        Span::styled(format!("  {status}{status_pad}"), status_style),
                        Span::styled(format!("  {elapsed:>TIME_WIDTH$}"), dim),
                    ]);
                } else {
                    let mut metadata = if width >= 16 {
                        format!(" · {}", truncate(&status, width - 12))
                    } else {
                        String::new()
                    };
                    if width >= 44 && metadata.width() + elapsed.width() + 11 <= width {
                        metadata.push_str(&format!(" · {elapsed}"));
                    }
                    let label = truncate(&label, width.saturating_sub(2 + metadata.width()));
                    spans.push(Span::styled(label, row_style));
                    spans.push(Span::styled(metadata, status_style));
                }
                let line = Line::from(spans);
                if is_selected {
                    line.style(Style::default().bg(theme.selected_bg))
                } else {
                    line
                }
            }
        };
        let y = body_y + i as u16;
        if let Some(background) = line.style.bg {
            buf.set_style(
                Rect::new(area.x, y, area.width, 1),
                Style::default().bg(background),
            );
        }
        buf.set_line(area.x, y, &line, area.width);
    }
}
