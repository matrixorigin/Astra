//! Detail-mode rendering: output tail, timing, and action hints.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::types::{
    BackgroundTaskKind, BackgroundTaskRow, DETAIL_TAIL_LINES, detail_actions_label, format_elapsed,
    format_timestamp_ms, pluralize_with_count,
};

pub(crate) fn render_detail(
    row: Option<&BackgroundTaskRow>,
    area: Rect,
    buf: &mut Buffer,
    fallback: impl FnOnce(Rect, &mut Buffer),
) {
    let Some(row) = row else {
        fallback(area, buf);
        return;
    };
    let dim = Style::default().fg(Color::DarkGray);
    let title_style = Style::default()
        .fg(row.status.color())
        .add_modifier(Modifier::BOLD);
    let mut timing_parts = Vec::new();
    if let Some(started_at_ms) = row.started_at_ms {
        timing_parts.push(format!("started {}", format_timestamp_ms(started_at_ms)));
    }
    if let Some(ended_at_ms) = row.ended_at_ms {
        timing_parts.push(format!("ended {}", format_timestamp_ms(ended_at_ms)));
    }
    timing_parts.push(format!("elapsed {}", format_elapsed(row.elapsed_ms)));

    let title_label = match row.kind {
        BackgroundTaskKind::Shell => "  command ",
        BackgroundTaskKind::LocalAgent => "  task ",
        BackgroundTaskKind::CloudSession
        | BackgroundTaskKind::MainSession
        | BackgroundTaskKind::Monitor => "  title ",
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("  {} · {}", row.id, row.status.label()),
            title_style,
        )),
        Line::from(vec![
            Span::styled("  kind ", dim),
            Span::raw(row.kind.as_str()),
            Span::styled(" · ", dim),
            Span::raw(timing_parts.join(" · ")),
        ]),
        Line::from(vec![
            Span::styled(title_label, dim),
            Span::raw(row.title.clone()),
        ]),
    ];
    if let Some(total_bytes) = row.total_bytes {
        let mut output_parts = Vec::new();
        if let Some(offset) = row.output_offset {
            output_parts.push(format!("offset {offset} -> {total_bytes}"));
        }
        output_parts.push(format!("{total_bytes} bytes"));
        if let Some(total_lines) = row.total_lines {
            output_parts.push(pluralize_with_count(total_lines as usize, "line", "lines"));
        }
        lines.push(Line::from(vec![
            Span::styled("  output ", dim),
            Span::raw(output_parts.join(" · ")),
        ]));
    }
    if let Some(exit_code) = row.exit_code {
        lines.push(Line::from(vec![
            Span::styled("  exit ", dim),
            Span::raw(exit_code.to_string()),
        ]));
    }
    if let Some(reason) = row.terminal_reason.as_deref() {
        lines.push(Line::from(vec![
            Span::styled("  reason ", dim),
            Span::raw(reason.to_string()),
        ]));
    }
    if let Some(label) = row.live_control.label() {
        lines.push(Line::from(vec![
            Span::styled("  control ", dim),
            Span::raw(label.to_string()),
        ]));
    }
    if let Some(output_ref) = row.output_ref.as_deref() {
        lines.push(Line::from(vec![
            Span::styled("  ref ", dim),
            Span::raw(output_ref.to_string()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  Tail", dim)));
    let tail = row
        .output_tail
        .as_deref()
        .map(str::trim_end)
        .filter(|tail| !tail.is_empty())
        .unwrap_or_else(|| row.status.empty_output_state());
    for line in tail.lines().take(DETAIL_TAIL_LINES) {
        lines.push(Line::from(format!("  {}", line)));
    }
    if row.status.is_killable() && row.live_control.can_stop() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            detail_actions_label(row.kind.supports_output_action(), true, area.width as usize),
            dim,
        )));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            detail_actions_label(
                row.kind.supports_output_action(),
                false,
                area.width as usize,
            ),
            dim,
        )));
    }

    for (i, line) in lines.into_iter().take(area.height as usize).enumerate() {
        buf.set_line(area.x, area.y + i as u16, &line, area.width);
    }
}
