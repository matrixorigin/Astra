//! Agent view helpers for the TUI event loop.
//!
//! Functions for opening, refreshing, and managing the agent-run navigator
//! and run-conversation views in the bottom pane.

use astra_turn_core::agent_live_event::AgentLiveEvent;
use astra_turn_core::agent_live_event::AgentLiveEventKind;

use super::app_event::TuiAppEvent;
use super::bottom_pane::BottomPane;
use super::bottom_pane::in_flight_agents_view::InFlightAgentsView;
use super::chat_widget;
use super::frame_requester::FrameRequester;

pub(crate) fn open_agents_view(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    let snapshot = chat_widget.agent_workbench_snapshot();
    if !snapshot.should_open() {
        return false;
    }
    if bottom_pane.activate_agent_monitor() {
        bottom_pane.refresh_agent_monitor(snapshot);
        return true;
    }
    bottom_pane.push_view(Box::new(InFlightAgentsView::new(snapshot)));
    bottom_pane.sync_popups();
    true
}

pub(crate) fn reopen_agents_view(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> bool {
    if !open_agents_view(chat_widget, bottom_pane) {
        return false;
    }
    frame_requester.schedule_frame();
    true
}

pub(crate) fn refresh_open_agent_detail_for_event(
    ae: &TuiAppEvent,
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    let rebound_pending_transcript = matches!(
        ae,
        TuiAppEvent::AgentLive(_) | TuiAppEvent::AgentLiveBatch(_)
    ) && bottom_pane.has_pending_agent_transcript_identity()
        && bottom_pane.refresh_agent_monitor(chat_widget.agent_workbench_snapshot());

    // The conversation view consumes the original typed live event. A task
    // card is only a compact navigator summary; it must never fill gaps in a
    // run transcript.
    let transcript_updated = match ae {
        TuiAppEvent::AgentLive(event) => bottom_pane.refresh_agent_live_event(event),
        TuiAppEvent::AgentLiveBatch(events) => {
            let mut updated = false;
            for event in events {
                updated |= bottom_pane.refresh_agent_live_event(event);
            }
            updated
        }
        TuiAppEvent::AgentLiveGap(gap) => bottom_pane.refresh_agent_live_gap(gap),
        _ => false,
    };

    let Some(open_id) = bottom_pane.active_live_task_id() else {
        return rebound_pending_transcript || transcript_updated;
    };
    let task_updated = match ae {
        TuiAppEvent::AgentLive(event) => {
            if open_id != event.agent_id {
                return rebound_pending_transcript || transcript_updated;
            }
            refresh_open_agent_detail_by_id(&event.agent_id, chat_widget, bottom_pane)
        }
        TuiAppEvent::AgentLiveBatch(events) => {
            let Some(event) = events.iter().rev().find(|event| open_id == event.agent_id) else {
                return rebound_pending_transcript || transcript_updated;
            };
            refresh_open_agent_detail_by_id(&event.agent_id, chat_widget, bottom_pane)
        }
        TuiAppEvent::AgentLiveGap(_) => false,
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
    };
    rebound_pending_transcript || transcript_updated || task_updated
}

pub(crate) fn agent_live_event_affects_monitor_row(event: &AgentLiveEvent) -> bool {
    matches!(
        event.kind,
        AgentLiveEventKind::ToolStarted { .. }
            | AgentLiveEventKind::ToolCompleted { .. }
            | AgentLiveEventKind::Signal(_)
            | AgentLiveEventKind::AgentTerminated { .. }
    )
}

pub(crate) fn agent_event_affects_monitor_rows(ae: &TuiAppEvent) -> bool {
    match ae {
        TuiAppEvent::AgentLive(event) => agent_live_event_affects_monitor_row(event),
        TuiAppEvent::AgentLiveBatch(events) => {
            events.iter().any(agent_live_event_affects_monitor_row)
        }
        TuiAppEvent::AgentLiveGap(_) => true,
        TuiAppEvent::AgentControlStarted { .. } | TuiAppEvent::AgentControlCompleted { .. } => true,
        TuiAppEvent::AgentCommunication(_) => true,
        _ => false,
    }
}

pub(crate) fn refresh_open_agent_monitor_for_event(
    ae: &TuiAppEvent,
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    if !bottom_pane.has_agent_monitor() || !agent_event_affects_monitor_rows(ae) {
        return false;
    }
    refresh_open_agent_monitor(chat_widget, bottom_pane)
}

pub(crate) fn refresh_open_agent_monitor(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    if !bottom_pane.has_agent_monitor() {
        return false;
    }
    bottom_pane.refresh_agent_monitor(chat_widget.agent_workbench_snapshot())
}

pub(crate) fn refresh_open_agent_views(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
) -> bool {
    let active_agent_id = bottom_pane.active_live_task_id().map(str::to_owned);
    let detail = active_agent_id.is_some_and(|agent_id| {
        refresh_open_agent_detail_by_id(&agent_id, chat_widget, bottom_pane)
    });
    let monitor = refresh_open_agent_monitor(chat_widget, bottom_pane);
    detail || monitor
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use astra_turn_core::agent_live_event::AgentLiveSignal;
    use crossterm::event::KeyEvent;
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::*;
    use crate::tui::bottom_pane::view::BottomPaneView;

    struct LiveEventRecorder(Arc<Mutex<Vec<String>>>);

    impl BottomPaneView for LiveEventRecorder {
        fn render(&self, _area: Rect, _buf: &mut Buffer) {}

        fn desired_height(&self, _width: u16) -> u16 {
            1
        }

        fn handle_key(&mut self, _key: KeyEvent) {}

        fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
            None
        }

        fn refresh_agent_live_event(&mut self, event: &AgentLiveEvent) -> bool {
            self.0.lock().unwrap().push(event.agent_id.clone());
            true
        }
    }

    #[test]
    fn batch_delivery_reaches_the_open_transcript_without_dropping_later_events() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(LiveEventRecorder(seen.clone())));
        let chat_widget = chat_widget::ChatWidget::new(String::new());

        let batch = TuiAppEvent::AgentLiveBatch(vec![
            AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "agent-1".into(),
                kind: AgentLiveEventKind::OutputDelta("first".into()),
            },
            AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "agent-1".into(),
                kind: AgentLiveEventKind::OutputDelta("second".into()),
            },
        ]);

        assert!(refresh_open_agent_detail_for_event(
            &batch,
            &chat_widget,
            &mut bottom_pane,
        ));
        assert_eq!(seen.lock().unwrap().as_slice(), ["agent-1", "agent-1"]);
    }

    #[test]
    fn pending_transcript_rebinds_before_delivering_first_token_event() {
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                run_id: "run-child".into(),
                agent_id: "reviewer@run-child".into(),
                kind: AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
                    parent_run_id: Some("run-root".into()),
                    depth: 1,
                    spawn_tool_call_id: Some("call-spawn-child".into()),
                    transcript_location: astra_turn_types::AgentTranscriptLocation::LocalJournal,
                }),
            }),
        ));

        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(
            crate::tui::bottom_pane::agent_transcript_view::AgentTranscriptView::live_unbound(
                "pending:call-spawn-child".into(),
                "Mock child review".into(),
                String::new(),
                None,
                "agents",
                80,
                12,
            ),
        ));

        let output = TuiAppEvent::AgentLive(AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "reviewer@run-child".into(),
            kind: AgentLiveEventKind::OutputDelta("child_evidence_visible".into()),
        });
        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(match &output {
                TuiAppEvent::AgentLive(event) => event.clone(),
                _ => unreachable!(),
            }),
        ));

        assert!(refresh_open_agent_detail_for_event(
            &output,
            &chat_widget,
            &mut bottom_pane,
        ));

        let area = Rect::new(0, 0, 80, 12);
        let mut buffer = Buffer::empty(area);
        bottom_pane.render(area, &mut buffer);
        let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(rendered.contains("child_evidence_visible"), "{rendered}");
    }
}
