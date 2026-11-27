//! Detail-mode rendering: output tail, timing, and action hints.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::types::{
    BackgroundTaskRow, BackgroundTaskStatusExt, DETAIL_TAIL_LINES, LiveControlState,
    format_elapsed, format_timestamp_ms, pluralize_with_count,
};

pub(crate) fn render_detail(
    row: Option<&BackgroundTaskRow>,
    area: Rect,
    buf: &mut Buffer,
    requested_scroll: usize,
    fallback: impl FnOnce(Rect, &mut Buffer),
) -> usize {
    let Some(row) = row else {
        fallback(area, buf);
        return 0;
    };
    let theme = crate::tui::theme::current();
    let dim = Style::default().fg(theme.dim);
    let title_style = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
    let mut timing_parts = Vec::new();
    if let Some(started_at_ms) = row.started_at_ms {
        timing_parts.push(format!("started {}", format_timestamp_ms(started_at_ms)));
    }
    if let Some(ended_at_ms) = row.ended_at_ms {
        timing_parts.push(format!("ended {}", format_timestamp_ms(ended_at_ms)));
    }
    timing_parts.push(format!("elapsed {}", format_elapsed(row.elapsed_ms)));

    let stale = row.live_control == LiveControlState::StaleHandle;
    let status = if stale {
        format!("stale · {}", row.status.label())
    } else {
        row.status.label().to_string()
    };
    let status_color = if stale {
        theme.warn
    } else {
        row.status.color()
    };
    let title = if row.title.trim().is_empty() {
        &row.id
    } else {
        &row.title
    };
    let width = usize::from(area.width);
    let status = crate::tui::history_cell::truncate_by_width(&status, width.saturating_sub(5) / 2);
    let separator = if status.is_empty() { "" } else { " · " };
    let title_width = width.saturating_sub(2 + separator.width() + status.width());
    let title = super::super::chat_composer::input_preview(title, title_width);
    let title_line = Line::from(vec![
        Span::raw("  "),
        Span::styled(title, title_style),
        Span::styled(separator, dim),
        Span::styled(status, Style::default().fg(status_color)),
    ]);
    let context_lines = vec![
        Line::from(vec![
            Span::styled("  kind ", dim),
            Span::raw(row.kind.as_str()),
            Span::styled(" · ", dim),
            Span::raw(timing_parts.join(" · ")),
        ]),
        Line::from(vec![Span::styled("  id ", dim), Span::raw(row.id.clone())]),
    ];
    let mut secondary_lines = Vec::new();
    if let Some(total_bytes) = row.total_bytes {
        let mut output_parts = Vec::new();
        if let Some(offset) = row.output_offset {
            output_parts.push(format!("offset {offset} -> {total_bytes}"));
        }
        output_parts.push(format!("{total_bytes} bytes"));
        if let Some(total_lines) = row.total_lines {
            output_parts.push(pluralize_with_count(total_lines as usize, "line", "lines"));
        }
        secondary_lines.push(Line::from(vec![
            Span::styled("  output ", dim),
            Span::raw(output_parts.join(" · ")),
        ]));
    }
    if let Some(exit_code) = row.exit_code {
        secondary_lines.push(Line::from(vec![
            Span::styled("  exit ", dim),
            Span::raw(exit_code.to_string()),
        ]));
    }
    let mut actionable_lines = Vec::new();
    if let Some(reason) = row.terminal_reason.as_deref() {
        actionable_lines.push(Line::from(vec![
            Span::styled("  reason ", dim),
            Span::raw(reason.to_string()),
        ]));
    }
    if let Some(label) = row.live_control.label() {
        actionable_lines.push(Line::from(vec![
            Span::styled("  control ", dim),
            Span::raw(label.to_string()),
        ]));
    }
    if let Some(inactive_ms) = row.no_recent_output_ms {
        actionable_lines.push(Line::from(vec![
            Span::styled("  activity ", dim),
            Span::raw(format!(
                "no output observed for {} · advisory only",
                format_elapsed(inactive_ms)
            )),
        ]));
    }
    if let Some(output_ref) = row.output_ref.as_deref() {
        actionable_lines.push(Line::from(vec![
            Span::styled("  ref ", dim),
            Span::raw(output_ref.to_string()),
        ]));
    }
    if let Some((stdout, stderr)) = &row.local_capture_paths {
        actionable_lines.push(Line::from(vec![
            Span::styled("  local stdout ", dim),
            Span::raw(stdout.display().to_string()),
        ]));
        actionable_lines.push(Line::from(vec![
            Span::styled("  local stderr ", dim),
            Span::raw(stderr.display().to_string()),
        ]));
    }

    // Keep diagnostics and the next useful action ahead of low-signal
    // counters. The detail viewport is intentionally short, so silently
    // keeping insertion order could hide the reason a task needs attention.
    let mut metadata_lines =
        Vec::with_capacity(actionable_lines.len() + context_lines.len() + secondary_lines.len());
    metadata_lines.extend(actionable_lines);
    metadata_lines.extend(context_lines);
    metadata_lines.extend(secondary_lines);

    let output = row
        .output_tail
        .as_deref()
        .map(str::trim_end)
        .filter(|tail| !tail.is_empty());
    let tail = output.unwrap_or(if stale {
        "No output retained · last known status"
    } else {
        row.status.empty_output_state()
    });
    let tail_lines = tail
        .lines()
        .rev()
        .take(DETAIL_TAIL_LINES)
        .collect::<Vec<_>>();
    let body_height = usize::from(area.height);
    let tail_capacity = body_height
        .saturating_sub(3)
        .min(DETAIL_TAIL_LINES)
        .min(tail_lines.len());
    let tail_budget = if tail_capacity > 0 {
        tail_capacity + 2
    } else {
        0
    };
    let metadata_capacity = body_height.saturating_sub(tail_budget + 1);
    let max_scroll = if metadata_capacity > 0 {
        metadata_lines.len().saturating_sub(metadata_capacity)
    } else {
        0
    };
    let scroll = requested_scroll.min(max_scroll);
    let mut lines = Vec::with_capacity(body_height);
    lines.push(title_line);
    lines.extend(
        metadata_lines
            .into_iter()
            .skip(scroll)
            .take(metadata_capacity),
    );
    if tail_capacity > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  Latest output", dim)));
        for line in tail_lines[..tail_capacity].iter().rev() {
            lines.push(Line::styled(
                format!("  {line}"),
                if output.is_some() {
                    Style::default().fg(theme.fg)
                } else {
                    dim
                },
            ));
        }
    }

    for (i, line) in lines.into_iter().take(body_height).enumerate() {
        buf.set_line(area.x, area.y + i as u16, &line, area.width);
    }
    scroll
}
