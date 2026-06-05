//! Translator from the legacy `TuiAppEvent` wire format to the
//! new `AppEvent` the `ChatWidget` consumes.
//!
//! Kept as a pure function so the swap in Phase 3d/3e can be
//! audited + unit-tested independently of the async loop. Both
//! formats will coexist during the migration; this module is the
//! one place where the mapping is declared, so later removal is a
//! one-file delete.

use super::{AppEvent, TurnStats, WireEvent};
use crate::tui::app_event::TuiAppEvent;

/// Context that the loop carries alongside each event, used to
/// populate `TurnComplete` stats. Kept separate from the event
/// payload so the enum stays small.
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnContext {
    pub elapsed_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub tools: u32,
    pub cumulative_tokens: Option<u64>,
    pub cumulative_cost_usd: Option<f64>,
}

impl TurnContext {
    fn into_stats(self) -> TurnStats {
        TurnStats {
            elapsed_ms: self.elapsed_ms,
            ttft_ms: self.ttft_ms,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cache_read_tokens: self.cache_read_tokens,
            tools: self.tools,
            cumulative_tokens: self.cumulative_tokens,
            cumulative_cost_usd: self.cumulative_cost_usd,
        }
    }
}

/// Translate a single `TuiAppEvent` into the new `AppEvent` the
/// `ChatWidget` understands. `ctx` is consulted ONLY for
/// `TurnComplete`; callers pass `TurnContext::default()` for
/// other variants.
///
/// Events that don't map onto a ChatWidget concern (bottom-pane
/// status like `WaitingForModel` / `ModelResponding` /
/// `StatusLine` / `ThinkingStarted` / `ThinkingStopped`) return
/// `None` — the caller keeps handling them itself. This makes
/// the migration additive: the legacy loop stays responsible for
/// the bottom pane, the new loop handles scrollback.
pub(crate) fn translate(ev: TuiAppEvent, ctx: TurnContext) -> Option<AppEvent> {
    match ev {
        TuiAppEvent::Token(text) => Some(AppEvent::Wire(WireEvent::AnswerDelta(text))),
        TuiAppEvent::ThinkingChunk(text) => Some(AppEvent::Wire(WireEvent::ReasoningDelta(text))),
        TuiAppEvent::ThinkingStopped => Some(AppEvent::Wire(WireEvent::ReasoningDone)),
        TuiAppEvent::ToolStarted {
            name,
            description,
            tool_use_id,
            parent_tool_use_id,
        } => Some(AppEvent::Wire(WireEvent::ToolStarted {
            name,
            description,
            tool_use_id,
            parent_tool_use_id,
        })),
        TuiAppEvent::AgentControlStarted {
            action,
            label,
            tool_use_id,
            agent_id,
        } => Some(AppEvent::Wire(WireEvent::AgentControlStarted {
            action,
            label,
            tool_use_id,
            agent_id,
        })),
        TuiAppEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
            parent_tool_use_id,
        } => Some(AppEvent::Wire(WireEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
            parent_tool_use_id,
        })),
        TuiAppEvent::AgentControlCompleted {
            action,
            label,
            status,
            duration_ms,
            output,
            tool_use_id,
            agent_id,
        } => Some(AppEvent::Wire(WireEvent::AgentControlCompleted {
            action,
            label,
            status,
            duration_ms,
            output,
            tool_use_id,
            agent_id,
        })),
        TuiAppEvent::ToolOutput { name, lines, bytes } => {
            Some(AppEvent::Wire(WireEvent::ToolOutput { name, lines, bytes }))
        }
        TuiAppEvent::AgentLive(event) => Some(AppEvent::Wire(WireEvent::AgentLive(event))),
        TuiAppEvent::AgentLiveBatch(events) => {
            Some(AppEvent::Wire(WireEvent::AgentLiveBatch(events)))
        }
        TuiAppEvent::TurnComplete => Some(AppEvent::Wire(WireEvent::TurnComplete(Box::new(
            ctx.into_stats(),
        )))),
        TuiAppEvent::TurnError(msg) => Some(AppEvent::Wire(WireEvent::TurnError(msg))),
        TuiAppEvent::ExplainReport(items) => Some(AppEvent::Wire(WireEvent::ExplainReport(items))),
        TuiAppEvent::VerdictReport(items) => Some(AppEvent::Wire(WireEvent::VerdictReport(items))),
        TuiAppEvent::Compaction(event) => Some(AppEvent::Wire(WireEvent::Compaction(event))),
        // Bottom-pane-only events — ChatWidget doesn't care.
        TuiAppEvent::ThinkingStarted
        | TuiAppEvent::WaitingForModel
        | TuiAppEvent::ModelResponding
        | TuiAppEvent::StatusLine(_)
        | TuiAppEvent::PermissionAutoApproved { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_to_answer_delta() {
        let out = translate(TuiAppEvent::Token("hi".into()), TurnContext::default());
        assert!(matches!(out, Some(AppEvent::Wire(WireEvent::AnswerDelta(s))) if s == "hi"));
    }

    #[test]
    fn thinking_chunk_to_reasoning_delta() {
        let out = translate(
            TuiAppEvent::ThinkingChunk("x".into()),
            TurnContext::default(),
        );
        assert!(matches!(out, Some(AppEvent::Wire(WireEvent::ReasoningDelta(s))) if s == "x"));
    }

    #[test]
    fn thinking_stopped_to_reasoning_done() {
        let out = translate(TuiAppEvent::ThinkingStopped, TurnContext::default());
        assert!(matches!(
            out,
            Some(AppEvent::Wire(WireEvent::ReasoningDone))
        ));
    }

    #[test]
    fn tool_events_preserve_fields() {
        let started = translate(
            TuiAppEvent::ToolStarted {
                name: "bash".into(),
                description: "ls".into(),
                tool_use_id: "tu_test".into(),
                parent_tool_use_id: None,
            },
            TurnContext::default(),
        );
        assert!(
            matches!(&started, Some(AppEvent::Wire(WireEvent::ToolStarted { name, description, tool_use_id, parent_tool_use_id }))
                if name == "bash" && description == "ls" && tool_use_id == "tu_test" && parent_tool_use_id.is_none())
        );

        let completed = translate(
            TuiAppEvent::ToolCompleted {
                name: "bash".into(),
                description: String::new(),
                status: "success".into(),
                duration_ms: 42,
                output_summary: Some("ok".into()),
                output: None,
                tool_use_id: "tu_test".into(),
                parent_tool_use_id: None,
            },
            TurnContext::default(),
        );
        match &completed {
            Some(AppEvent::Wire(WireEvent::ToolCompleted {
                status,
                duration_ms: 42,
                ..
            })) => assert_eq!(status, "success"),
            other => panic!("unexpected tool completed event: {other:?}"),
        }
    }

    #[test]
    fn turn_complete_attaches_context_stats() {
        let ctx = TurnContext {
            elapsed_ms: Some(1_500),
            ttft_ms: Some(400),
            tokens_in: Some(200),
            tokens_out: Some(50),
            cache_read_tokens: None,
            tools: 2,
            cumulative_tokens: Some(250),
            cumulative_cost_usd: Some(0.014),
        };
        let out = translate(TuiAppEvent::TurnComplete, ctx);
        match out {
            Some(AppEvent::Wire(WireEvent::TurnComplete(stats))) => {
                assert_eq!(stats.elapsed_ms, Some(1_500));
                assert_eq!(stats.ttft_ms, Some(400));
                assert_eq!(stats.tools, 2);
                assert_eq!(stats.cumulative_cost_usd, Some(0.014));
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
    }

    #[test]
    fn turn_error_carries_message() {
        let out = translate(
            TuiAppEvent::TurnError("rate limited".into()),
            TurnContext::default(),
        );
        assert!(
            matches!(out, Some(AppEvent::Wire(WireEvent::TurnError(s))) if s == "rate limited")
        );
    }

    #[test]
    fn bottom_pane_events_return_none() {
        // Documents the contract: events that don't concern
        // scrollback must be explicitly ignored here so the
        // caller routes them to BottomPane instead of letting
        // them fall through.
        for ev in [
            TuiAppEvent::ThinkingStarted,
            TuiAppEvent::WaitingForModel,
            TuiAppEvent::ModelResponding,
            TuiAppEvent::StatusLine("hello".into()),
        ] {
            assert!(
                translate(ev.clone(), TurnContext::default()).is_none(),
                "expected None for {ev:?}"
            );
        }
    }
}
