#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

mod agent_control_status;
mod agent_run_projection;
mod agent_view;
mod app_event;
pub(crate) mod approval;
mod background_shortcut;
pub(crate) mod background_tasks;
mod bg_task_proxy;
pub(crate) mod bg_task_rendering;
mod board_pin;
mod bottom_pane;
mod config_edit_router;
mod context_panel;
// Core (post-refactor): HistoryCell trait + TurnEvent schema +
// single ChatWidget router + on-disk JSONL transcript. See
// `docs/design/tui-refactor.md`.
mod chat_widget;
mod color;
mod custom_terminal;
mod diff_render;
mod draw;
mod event;
mod event_loop;
mod external_editor;
mod file_writer;
mod frame_rate_limiter;
mod frame_requester;
mod glyphs;
mod history_cell;
mod insert_history;
mod keymap;
mod local_agent_journal;
mod local_agent_snapshot;
mod markdown;
mod markdown_render;
mod mention_menu;
pub(crate) mod path_style;
mod plan_mode;
mod plan_task_observer;
mod render;
mod server_agent_observer;
mod session_picker;
mod shimmer;
mod slash_dispatch;
pub(crate) mod slash_menu;

/// Unified slash/command fuzzy scorer — single source of truth for
/// ranking across the inline `/cmd` popup, Tab-completion for
/// subcommands, and "did you mean?" hints in the CLI layer.
#[inline]
pub(crate) fn score_slash_token(needle: &str, haystack: &str) -> Option<u32> {
    slash_menu::score_token(needle, haystack)
}

/// Width-aware string truncation that appends `…` when the string
/// exceeds `max_chars`. Used by slash and mention popups.
pub(crate) fn truncate_ellipsis(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let out: String = s.chars().take(max_chars - 1).collect();
    format!("{out}…")
}

mod status_indicator;
mod status_line;
mod stream_bridge;
mod style;
mod task_board_events;
pub(crate) mod task_board_observer;
mod task_list;
mod task_status;
mod terminal;
mod terminal_palette;
mod theme;
mod timeline;
pub(crate) mod turn_event;
pub(crate) mod ui_adapter;
pub(crate) mod work_board_projection;
mod worktrees;
mod wrapping;

pub(crate) use event_loop::{can_run_tui, run_tui_session as run_tui};

/// Walk the chat widget's committed history and emit role/text
/// pairs for the `/context dump` JSON file.
pub(crate) fn collect_chat_turns_for_dump(
    chat: &chat_widget::ChatWidget,
) -> Vec<crate::cli::context_dump::ChatTurnDump> {
    use crate::cli::context_dump::ChatTurnDump;
    use history_cell::{
        assistant::AssistantCell, reasoning::ReasoningCell, system::SystemCell, user::UserCell,
    };
    let mut out = Vec::new();
    for cell in chat.history() {
        let any = cell.as_any_ref();
        if let Some(u) = any.downcast_ref::<UserCell>() {
            out.push(ChatTurnDump {
                role: "user".into(),
                text: u.text().to_string(),
            });
        } else if let Some(a) = any.downcast_ref::<AssistantCell>() {
            out.push(ChatTurnDump {
                role: "assistant".into(),
                text: a.source().to_string(),
            });
        } else if let Some(r) = any.downcast_ref::<ReasoningCell>() {
            out.push(ChatTurnDump {
                role: "reasoning".into(),
                text: r.text().to_string(),
            });
        } else if any.downcast_ref::<SystemCell>().is_some() {
            // System cells are UI chrome — skip.
        }
    }
    out
}
