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
            BackgroundTaskListEntry::FanoutHeader(header) => {
                fanout_header_line(header, dim, usize::from(area.width))
            }
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
                        .fg(theme.selected_fg)
                        .add_modifier(Modifier::BOLD)
                } else if row.display_priority() == 2 {
                    dim
                } else {
                    Style::default().fg(theme.fg)
                };
                visible_row_number += 1;
                let marker = if is_selected { "› " } else { "  " };
                let index = format!(
                    "{visible_row_number}. {} ",
                    crate::tui::style::status_glyph(row.status.presentation())
                );
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
                let event_time = if area.width >= 88 {
                    row.ended_at_ms
                        .or(row.started_at_ms)
                        .and_then(|ms| i64::try_from(ms).ok())
                        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                        .map(|time| format!("  {}", time.format("%m-%d %H:%MZ")))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let attention = if row.display_priority() == 2 {
                    "  history"
                } else {
                    ""
                };
                let width = usize::from(area.width);
                let elapsed = format_elapsed(row.elapsed_ms);
                let prefix = if row.live_control == LiveControlState::StaleHandle {
                    "stale · "
                } else {
                    ""
                };
                let mut meta = format!(
                    "{}  {prefix}{}  {elapsed}",
                    row.kind.as_str(),
                    row.status.label()
                );
                let exit = row
                    .exit_code
                    .map(|code| format!("  exit {code}"))
                    .unwrap_or_default();
                // Keep a recognizable title alongside status and duration as
                // the pane narrows. Full identity and facts are in details.
                let budget = width.saturating_sub(marker.width() + index.width() + 16);
                for extra in [
                    exit,
                    control,
                    ownership.to_string(),
                    activity,
                    event_time,
                    attention.to_string(),
                ] {
                    if meta.width() + extra.width() <= budget {
                        meta.push_str(&extra);
                    }
                }
                meta.push_str("  ");
                let title = if *grouped {
                    fanout_slot_title(row)
                } else {
                    row.title.clone()
                };
                let (id, title) =
                    compact_list_row_text(row, title.as_str(), width, marker, &index, &meta);
                let line = Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(index, Style::default().fg(row.status.color())),
                    Span::styled(id, row_style),
                    Span::styled("  ", dim),
                    Span::styled(meta, dim),
                    Span::styled(title, row_style),
                ]);
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
    let fixed_without_id_or_title = marker.width() + index.width() + 2 + meta.width();
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
    let id = crate::tui::history_cell::truncate_by_width(&row.id, id_budget);
    let used = fixed_without_id_or_title + id.width();
    let title_budget = width.saturating_sub(used);
    let title = crate::tui::history_cell::truncate_by_width(title, title_budget);
    (id, title)
}
