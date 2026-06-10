//! Agent view helpers for the TUI event loop.
//!
//! Functions for opening, refreshing, and managing agent detail/monitor views
//! in the bottom pane.

use astra_turn_core::agent_live_event::AgentLiveEvent;
use astra_turn_core::agent_live_event::AgentLiveEventKind;

use super::app_event::TuiAppEvent;
use super::bottom_pane::in_flight_agents_view::InFlightAgentsView;
use super::bottom_pane::BottomPane;
use super::chat_widget;
use super::frame_requester::FrameRequester;

// Re-exported from event_loop.rs for backward compatibility
const AGENT_DRILLDOWN_RECENT_COMPLETED: usize = 5;

pub(crate) fn reopen_agents_view(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> bool {
    let rows = chat_widget.agents_drilldown_rows(AGENT_DRILLDOWN_RECENT_COMPLETED);
    if rows.is_empty() {
        return false;
    }
    bottom_pane.push_view(Box::new(InFlightAgentsView::new(rows)));
    bottom_pane.sync_popups();
    frame_requester.schedule_frame();
    true
}

pub(crate) fn refresh_open_agent_detail_for_event(
    ae: &TuiAppEvent,
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    let Some(open_id) = bottom_pane.active_live_task_id() else {
        return false;
    };
    match ae {
        TuiAppEvent::AgentLive(event) => {
            if open_id != event.agent_id {
                return false;
            }
            refresh_open_agent_detail_by_id(&event.agent_id, chat_widget, bottom_pane)
        }
        TuiAppEvent::AgentLiveBatch(events) => {
            let Some(event) = events.iter().rev().find(|event| open_id == event.agent_id) else {
                return false;
            };
            refresh_open_agent_detail_by_id(&event.agent_id, chat_widget, bottom_pane)
        }
        TuiAppEvent::AgentControlStarted {
            agent_id: Some(agent_id),
            ..
        }
        | TuiAppEvent::AgentControlCompleted {
            agent_id: Some(agent_id),
            ..
        } => {
            if open_id != agent_id {
                return false;
            }
            refresh_open_agent_detail_by_id(agent_id, chat_widget, bottom_pane)
        }
        _ => false,
    }
}

pub(crate) fn agent_live_event_affects_monitor_row(event: &AgentLiveEvent) -> bool {
    matches!(
        event.kind,
        AgentLiveEventKind::ToolStarted { .. }
            | AgentLiveEventKind::ToolCompleted { .. }
            | AgentLiveEventKind::AgentTerminated { .. }
    )
}

pub(crate) fn agent_event_affects_monitor_rows(ae: &TuiAppEvent) -> bool {
    match ae {
        TuiAppEvent::AgentLive(event) => agent_live_event_affects_monitor_row(event),
        TuiAppEvent::AgentLiveBatch(events) => {
            events.iter().any(agent_live_event_affects_monitor_row)
        }
        TuiAppEvent::AgentControlStarted { .. } | TuiAppEvent::AgentControlCompleted { .. } => true,
        _ => false,
    }
}

pub(crate) fn refresh_open_agent_monitor_for_event(
    ae: &TuiAppEvent,
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    if !bottom_pane.agent_monitor_is_open() || !agent_event_affects_monitor_rows(ae) {
        return false;
    }
    bottom_pane.refresh_agent_rows(chat_widget.agents_drilldown_rows(50))
}

pub(crate) fn refresh_open_agent_views_for_event(
    ae: &TuiAppEvent,
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    let detail = refresh_open_agent_detail_for_event(ae, chat_widget, bottom_pane);
    let monitor = refresh_open_agent_monitor_for_event(ae, chat_widget, bottom_pane);
    detail || monitor
}

pub(crate) fn refresh_open_agent_detail_by_id(
    agent_id: &str,
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    if let Some(cell) = chat_widget.task_cell_anywhere(agent_id) {
        bottom_pane.refresh_task_detail(agent_id, cell)
    } else {
        false
    }
}
