//! XML rendering for background task rows.
//!
//! Extracted from [`super::bg_task_proxy`].

pub(crate) fn xml_escape_attr(value: &str) -> String {
    astra_text_utils::xml_escape::xml_escape_attr(value).into_owned()
}

pub(crate) fn truncate_xml_attr(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub(crate) fn live_control_xml_value(
    live_control: crate::tui::bottom_pane::background_task_view::LiveControlState,
) -> &'static str {
    match live_control {
        crate::tui::bottom_pane::background_task_view::LiveControlState::Available => "available",
        crate::tui::bottom_pane::background_task_view::LiveControlState::StaleHandle => {
            "stale_handle"
        }
        crate::tui::bottom_pane::background_task_view::LiveControlState::UnsupportedInMode => {
            "unsupported_in_mode"
        }
    }
}

pub(crate) fn render_background_task_rows_xml(
    rows: &[crate::tui::bottom_pane::background_task_view::BackgroundTaskRow],
) -> String {
    use crate::tui::bottom_pane::background_task_view::BackgroundTaskKind;

    if rows.is_empty() {
        return "<background_tasks count=\"0\" />".to_string();
    }

    let mut rows = rows.to_vec();
    rows.sort_by_key(|row| {
        let attention_rank = match row.status {
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::WaitingForInput
            | crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Failed => 0,
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Running
            | crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Pending => 1,
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Killed => 2,
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Completed => 3,
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Unavailable => 4,
        };
        (
            attention_rank,
            row.started_at_ms.unwrap_or(u64::MAX),
            row.id.clone(),
        )
    });

    let mut out = format!("<background_tasks count=\"{}\">", rows.len());
    for row in rows {
        let mut attrs = vec![
            ("id", xml_escape_attr(&row.id)),
            ("kind", row.kind.as_str().to_string()),
            ("status", row.status.as_str().to_string()),
            (
                "live_control",
                live_control_xml_value(row.live_control).to_string(),
            ),
            ("elapsed_ms", row.elapsed_ms.to_string()),
            ("title", xml_escape_attr(&row.title)),
        ];
        match row.kind {
            BackgroundTaskKind::Shell => attrs.push(("command", xml_escape_attr(&row.title))),
            _ => attrs.push(("description", xml_escape_attr(&row.title))),
        }
        if let Some(started_at_ms) = row.started_at_ms {
            attrs.push(("started_at_ms", started_at_ms.to_string()));
        }
        if let Some(ended_at_ms) = row.ended_at_ms {
            attrs.push(("ended_at_ms", ended_at_ms.to_string()));
        }
        if let Some(output_ref) = row.output_ref.as_deref() {
            attrs.push(("output_ref", xml_escape_attr(output_ref)));
        }
        if let Some(output_offset) = row.output_offset {
            attrs.push(("output_offset", output_offset.to_string()));
        }
        if let Some(total_bytes) = row.total_bytes {
            attrs.push(("total_output_bytes", total_bytes.to_string()));
        }
        if let Some(total_lines) = row.total_lines {
            attrs.push(("total_output_lines", total_lines.to_string()));
        }
        if let Some(preview) = row
            .output_tail
            .as_deref()
            .and_then(|tail| tail.lines().next_back())
            .map(str::trim)
            .filter(|preview| !preview.is_empty())
        {
            attrs.push(("preview", xml_escape_attr(&truncate_xml_attr(preview, 160))));
        }
        if let Some(exit_code) = row.exit_code {
            attrs.push(("exit_code", exit_code.to_string()));
        }
        if let Some(reason) = row.terminal_reason.as_deref() {
            attrs.push(("terminal_reason", xml_escape_attr(reason)));
        }
        if let Some(fanout) = row.fanout.as_ref() {
            attrs.push(("fanout_group_id", xml_escape_attr(&fanout.group_id)));
            attrs.push(("fanout_group_title", xml_escape_attr(&fanout.group_title)));
            attrs.push(("fanout_target_count", fanout.target_count.to_string()));
            attrs.push(("fanout_slot_index", fanout.slot_index.to_string()));
            attrs.push(("fanout_slot_label", xml_escape_attr(&fanout.slot_label)));
        }

        out.push_str("\n<task");
        for (key, value) in attrs {
            out.push_str(&format!(" {key}=\"{value}\""));
        }
        out.push_str(" />");
    }
    out.push_str("\n</background_tasks>");
    out
}
