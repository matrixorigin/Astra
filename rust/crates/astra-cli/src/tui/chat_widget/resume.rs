//! Resume path — load a session's JSONL transcript into a
//! `ChatWidget`. Phase 4 of the refactor.
//!
//! The heavy lifting (parsing, decoding each `TurnEvent` into the
//! right `HistoryCell`) already lives in `transcript_jsonl::load`
//! and `ChatWidget::replay`. This module is the single-function
//! glue that wires them together so callers don't need to know
//! about the JSONL backing store — they pass a session id, they
//! get a populated widget back.

use super::super::transcript_jsonl;
use super::ChatWidget;

/// Load `session_id`'s transcript from disk and replay it into a
/// fresh widget. Missing file / empty session id / malformed lines
/// all collapse to "no history restored" — the widget is returned
/// empty, never an error, because a partial replay is better than
/// refusing to open the session.
///
/// The returned widget already has its `committed_watermark`
/// advanced past every replayed cell — the caller is expected to
/// paint those cells into the terminal scrollback exactly once via
/// its own renderer, then future `drain_new_committed` calls will
/// only surface new activity.
pub(crate) fn load(session_id: impl Into<String>) -> ChatWidget {
    let sid = session_id.into();
    let events = transcript_jsonl::load(&sid);
    let mut w = ChatWidget::new(sid);
    w.replay(events);
    // Important: anything we just replayed has already been
    // rendered to the terminal by the caller (or will be in this
    // same call); advancing the watermark here prevents those
    // cells from being redrawn when the widget's next tick runs
    // `drain_new_committed`.
    //
    // We deliberately do NOT call `mark_all_flushed` here though —
    // the caller is in a better position to decide when the paint
    // is actually complete (it may need to first render a banner,
    // open the terminal, etc.). `load` just hands back a
    // replay-populated widget; the flush is the caller's call.
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app_event::TuiAppEvent;
    use crate::tui::chat_widget::bridge::{TurnContext, translate};
    use crate::tui::history_cell::{
        HistoryCell, assistant::AssistantCell, turn_summary::TurnSummaryCell, user::UserCell,
    };
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use crate::tui::transcript_jsonl;
    use crate::tui::turn_event::TurnEvent;
    use ratatui::text::Line;

    /// Run `f` with `$HOME` pointing at a fresh tempdir so the
    /// append+load test doesn't scribble into the dev's real
    /// `~/.astra/`. Mirrors `transcript_jsonl::tests::with_tmp_home`
    /// but we can't re-use that helper (private).
    fn with_tmp_home<F: FnOnce()>(f: F) {
        use std::env;
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = env::var("HOME").ok();
        unsafe {
            env::set_var("HOME", tmp.path());
        }
        f();
        match prev {
            Some(v) => unsafe { env::set_var("HOME", v) },
            None => unsafe { env::remove_var("HOME") },
        }
    }

    fn render_history(w: &ChatWidget, width: u16) -> String {
        let mut all_lines: Vec<Line<'static>> = Vec::new();
        for (i, cell) in w.history().iter().enumerate() {
            if i > 0 {
                all_lines.push(Line::default());
            }
            all_lines.extend(cell.display_lines(width));
        }
        let height = (all_lines.len() as u16).max(1);
        let p = ratatui::widgets::Paragraph::new(all_lines)
            .wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, height))
    }

    #[test]
    #[serial_test::serial]
    fn round_trip_restores_cell_types_in_order() {
        // End-to-end: append a plausible turn's worth of events,
        // load them through the resume path, check that the widget
        // has the right cell types in the right order with the
        // original payloads intact.
        with_tmp_home(|| {
            let sid = "sess_resume_e2e";
            let events = vec![
                TurnEvent::User {
                    ts: None,
                    text: "what's up".into(),
                },
                TurnEvent::Assistant {
                    ts: None,
                    markdown: "# hi\n\nall good".into(),
                },
                TurnEvent::TurnSummary {
                    ts: None,
                    elapsed_ms: Some(1_200),
                    ttft_ms: Some(300),
                    tokens_in: Some(200),
                    tokens_out: Some(40),
                    cache_read_tokens: None,
                    tools: 0,
                    cumulative_tokens: Some(240),
                    cumulative_cost_usd: Some(0.001),
                },
            ];
            for e in &events {
                transcript_jsonl::append(sid, e);
            }

            let w = load(sid);
            assert_eq!(w.session_id(), sid);
            assert_eq!(
                w.history().len(),
                3,
                "three events → three cells; nothing dropped"
            );
            assert!(
                w.history()[0].as_any_ref().is::<UserCell>(),
                "first cell is the user message"
            );
            assert!(
                w.history()[1].as_any_ref().is::<AssistantCell>(),
                "second cell is the assistant reply"
            );
            assert!(
                w.history()[2].as_any_ref().is::<TurnSummaryCell>(),
                "third cell is the turn summary"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn translated_live_turn_survives_jsonl_resume_identically() {
        with_tmp_home(|| {
            let sid = "sess_live_resume_e2e";
            let mut live = ChatWidget::new(sid);

            live.handle_event(super::super::AppEvent::UserSubmit(
                "inspect cache stats".into(),
            ));
            for ev in [
                TuiAppEvent::ThinkingChunk("checking prior context".into()),
                TuiAppEvent::ThinkingStopped,
                TuiAppEvent::ToolStarted {
                    name: "bash".into(),
                    description: "echo cache".into(),
                },
                TuiAppEvent::ToolCompleted {
                    name: "bash".into(),
                    description: String::new(),
                    status: "success".into(),
                    duration_ms: 12,
                    output_summary: Some("cache ok".into()),
                    output: None,
                },
                TuiAppEvent::Token("Cache stats survived.".into()),
                TuiAppEvent::TurnComplete,
            ] {
                let ctx = TurnContext {
                    elapsed_ms: Some(1_500),
                    ttft_ms: Some(250),
                    tokens_in: Some(200),
                    tokens_out: Some(50),
                    cache_read_tokens: Some(150),
                    tools: 1,
                    cumulative_tokens: Some(250),
                    cumulative_cost_usd: Some(0.002),
                };
                if let Some(app_ev) = translate(ev, ctx) {
                    live.handle_event(app_ev);
                }
            }

            let persisted = transcript_jsonl::load(sid);
            assert_eq!(
                persisted.len(),
                live.history().len(),
                "every committed live cell should be persisted"
            );

            let resumed = load(sid);
            assert_eq!(resumed.history().len(), live.history().len());
            assert_eq!(render_history(&resumed, 80), render_history(&live, 80));

            let summary = resumed
                .history()
                .last()
                .and_then(|cell| cell.as_any_ref().downcast_ref::<TurnSummaryCell>())
                .expect("last resumed cell should be turn summary");
            assert_eq!(summary.cache_read_tokens, Some(150));
            let summary_text = summary
                .display_lines(120)
                .into_iter()
                .map(|line| {
                    line.spans
                        .into_iter()
                        .map(|span| span.content.into_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                summary_text.contains("💾 75%"),
                "cache-read stats must remain user-visible after resume"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn missing_file_yields_empty_widget() {
        // First-launch / never-seen session id → empty widget, not
        // a panic. This path is common on startup for brand-new
        // sessions that haven't recorded anything yet.
        with_tmp_home(|| {
            let w = load("does_not_exist_yet");
            assert!(w.history().is_empty());
            assert_eq!(w.session_id(), "does_not_exist_yet");
        });
    }

    #[test]
    #[serial_test::serial]
    fn empty_session_id_yields_empty_widget() {
        // Defensive: a blank id shouldn't reach the filesystem
        // (see `transcript_jsonl::transcript_path`). Verify the
        // returned widget is usable even though nothing was loaded.
        let w = load("");
        assert!(w.history().is_empty());
        assert_eq!(w.session_id(), "");
    }
}
