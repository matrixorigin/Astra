//! Gate evaluation: turn raw turn counters + session memory state into a
//! decision with a typed skip reason (so the sync path can emit a
//! precise `SessionMemoryExtraction{outcome="skipped"}` event instead of
//! a boolean).
//!
//! Thin wrapper over
//! [`astra_turn_core::cloud_session_memory_extract::should_extract_with_error_trigger`]
//! — keeps the reason inference co-located with the callers that log it.

use astra_services::session_journal::SessionMemoryExtractionSkipReason;
use astra_turn_core::cloud_session_memory_extract::{
    SessionMemoryExtractConfig, SessionMemoryState, should_extract_with_error_trigger,
};

/// One of: run, or skip with a reason.
#[derive(Debug, PartialEq, Eq)]
pub enum GateDecision {
    Run,
    Skip(SessionMemoryExtractionSkipReason),
}

/// Evaluate the gate for a prospective extraction. Pure function — the
/// selector-cooldown and in-flight checks live on the service, not here.
pub fn evaluate(
    state: &SessionMemoryState,
    session_id: &str,
    current_tokens: usize,
    current_tool_calls: usize,
    had_error: bool,
    had_user_correction: bool,
    config: &SessionMemoryExtractConfig,
) -> GateDecision {
    let decision = evaluate_inner(
        state,
        session_id,
        current_tokens,
        current_tool_calls,
        had_error,
        had_user_correction,
        config,
    );
    // Operator-facing trace: inspectable in production logs when
    // something looks off. Scoped to DEBUG so steady-state volume is
    // zero unless explicitly enabled. `session_id` is truncated to
    // avoid leaking into INFO streams.
    tracing::debug!(
        target: "astra_runtime::session_memory::gate",
        session_id = %truncate_sid(session_id),
        initialized = state.initialized,
        tokens_at_last = state.tokens_at_last_extraction,
        tool_calls_at_last = state.tool_calls_at_last_extraction,
        current_tokens,
        current_tool_calls,
        had_error,
        had_user_correction,
        min_tokens_to_init = config.min_tokens_to_init,
        min_tokens_between_updates = config.min_tokens_between_updates,
        min_tool_calls_between_updates = config.min_tool_calls_between_updates,
        decision = ?decision,
        "session-memory gate evaluated"
    );
    // Temporary diagnostic — uncomment if gate outcomes look wrong and
    // you can't get tracing subscribers attached (e.g. CLI REPL
    // without `--diagnostic-log`).
    if std::env::var("ASTRA_SESSION_MEMORY_TRACE").is_ok() {
        eprintln!(
            "[gate] sid={} init={} tokens_at_last={} current_tokens={} tool_calls_at_last={} current_tool_calls={} had_error={} had_user_correction={} min_init={} min_between={} decision={:?}",
            truncate_sid(session_id),
            state.initialized,
            state.tokens_at_last_extraction,
            current_tokens,
            state.tool_calls_at_last_extraction,
            current_tool_calls,
            had_error,
            had_user_correction,
            config.min_tokens_to_init,
            config.min_tokens_between_updates,
            decision,
        );
    }
    decision
}

/// Inner logic split out so `evaluate` can log inputs + outputs at one
/// boundary. Keeps the original code path unchanged.
fn evaluate_inner(
    state: &SessionMemoryState,
    session_id: &str,
    current_tokens: usize,
    current_tool_calls: usize,
    had_error: bool,
    had_user_correction: bool,
    config: &SessionMemoryExtractConfig,
) -> GateDecision {
    if session_id.is_empty() {
        return GateDecision::Skip(SessionMemoryExtractionSkipReason::NoSessionId);
    }

    // Errors always bypass the init gate. First-turn failures carry
    // debuggable context (stack trace, offending args) that the
    // session-memory L1 is specifically meant to capture — gating them
    // on "has the conversation reached 10K tokens yet?" silently drops
    // exactly the cases operators most want to postmortem. The prior
    // inline implementation always persisted on error, so this also
    // restores that behaviour.
    //
    // NOTE: the empty-sid check above must stay BEFORE this branch —
    // `had_error=true` with `session_id=""` is still NoSessionId, not
    // a runnable extraction. Do not reorder.
    if had_error {
        return GateDecision::Run;
    }

    // A user correction/reanchor means the previous session summary may now
    // contain stale intent. Refresh promptly so the next turn doesn't retrieve
    // old working memory that conflicts with the user's latest direction.
    if had_user_correction {
        return GateDecision::Run;
    }

    // The init gate fires before any growth delta can matter.
    if !state.initialized && current_tokens < config.min_tokens_to_init {
        return GateDecision::Skip(SessionMemoryExtractionSkipReason::BelowInitGate);
    }

    if should_extract_with_error_trigger(
        state,
        current_tokens,
        current_tool_calls,
        config,
        had_error,
    ) {
        GateDecision::Run
    } else {
        // Initialized + no growth delta + no error → straightforward
        // debounce.
        GateDecision::Skip(SessionMemoryExtractionSkipReason::NoGrowth)
    }
}

fn truncate_sid(sid: &str) -> &str {
    match sid.char_indices().nth(8) {
        Some((i, _)) => &sid[..i],
        None => sid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SessionMemoryExtractConfig {
        SessionMemoryExtractConfig::default()
    }

    #[test]
    fn empty_session_id_is_no_session_id() {
        let state = SessionMemoryState::default();
        let decision = evaluate(&state, "", 50_000, 10, false, false, &cfg());
        assert_eq!(
            decision,
            GateDecision::Skip(SessionMemoryExtractionSkipReason::NoSessionId)
        );
    }

    #[test]
    fn below_init_threshold_is_below_init_gate() {
        let state = SessionMemoryState::default();
        let decision = evaluate(&state, "sess-1", 5_000, 0, false, false, &cfg());
        assert_eq!(
            decision,
            GateDecision::Skip(SessionMemoryExtractionSkipReason::BelowInitGate)
        );
    }

    #[test]
    fn initialized_but_no_growth_is_no_growth() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 12_000,
            tool_calls_at_last_extraction: 5,
            last_extraction_time: None,
        };
        let decision = evaluate(&state, "sess-1", 13_000, 6, false, false, &cfg());
        assert_eq!(
            decision,
            GateDecision::Skip(SessionMemoryExtractionSkipReason::NoGrowth)
        );
    }

    #[test]
    fn initialized_token_growth_without_tool_growth_runs() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 12_000,
            tool_calls_at_last_extraction: 5,
            last_extraction_time: None,
        };
        let decision = evaluate(&state, "sess-1", 17_500, 5, false, false, &cfg());
        assert_eq!(decision, GateDecision::Run);
    }

    #[test]
    fn initialized_tool_growth_without_token_growth_runs() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 12_000,
            tool_calls_at_last_extraction: 5,
            last_extraction_time: None,
        };
        let decision = evaluate(&state, "sess-1", 12_500, 10, false, false, &cfg());
        assert_eq!(decision, GateDecision::Run);
    }

    #[test]
    fn past_init_gate_with_room_to_run_fires_run() {
        let state = SessionMemoryState::default();
        let decision = evaluate(&state, "sess-1", 50_000, 10, false, false, &cfg());
        assert_eq!(decision, GateDecision::Run);
    }

    #[test]
    fn had_error_past_init_overrides_debounce() {
        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 11_000,
            tool_calls_at_last_extraction: 4,
            last_extraction_time: None,
        };
        // Only +1K tokens and +1 tool call — normally debounced.
        let decision = evaluate(&state, "sess-1", 12_000, 5, true, false, &cfg());
        assert_eq!(decision, GateDecision::Run);
    }

    /// Regression: a first-turn failure below `min_tokens_to_init`
    /// used to be silently dropped by the init gate — the `had_error`
    /// override was only evaluated inside
    /// `should_extract_with_error_trigger`, which came AFTER the
    /// `BelowInitGate` early return. That regressed the pre-
    /// refactor inline behaviour (which always persisted L1 on
    /// error) and meant the most diagnostically valuable sessions
    /// never got captured. This test locks in the invariant that
    /// errors always run extraction, regardless of token count.
    #[test]
    fn had_error_on_uninitialized_session_bypasses_init_gate() {
        let state = SessionMemoryState::default();
        // Tokens well below min_tokens_to_init (10_000 default) and
        // state is uninitialized — normally BelowInitGate.
        let decision = evaluate(&state, "sess-1", 500, 0, true, false, &cfg());
        assert_eq!(
            decision,
            GateDecision::Run,
            "first-turn error must trigger extraction even below init gate"
        );
    }

    /// Companion: with `had_error=false`, the init gate still holds
    /// — the error bypass must not accidentally loosen the
    /// no-error debounce.
    #[test]
    fn no_error_below_init_still_skips() {
        let state = SessionMemoryState::default();
        let decision = evaluate(&state, "sess-1", 500, 0, false, false, &cfg());
        assert_eq!(
            decision,
            GateDecision::Skip(SessionMemoryExtractionSkipReason::BelowInitGate)
        );
    }

    #[test]
    fn user_correction_on_uninitialized_session_bypasses_init_gate() {
        let state = SessionMemoryState::default();
        let decision = evaluate(&state, "sess-1", 500, 0, false, true, &cfg());
        assert_eq!(
            decision,
            GateDecision::Run,
            "user correction must refresh session memory even below init gate"
        );
    }

    /// Regression: empty `session_id` + `had_error=true` must still
    /// short-circuit to `NoSessionId`, NOT slip through the error
    /// bypass and produce a `Run` with no sid. Persisting under "" is
    /// a phantom-session bug that silently collides across unrelated
    /// callers. The empty-sid check MUST precede the `had_error`
    /// branch — this test locks that ordering in.
    #[test]
    fn had_error_with_empty_session_id_is_no_session_id() {
        let state = SessionMemoryState::default();
        let decision = evaluate(&state, "", 50_000, 10, true, false, &cfg());
        assert_eq!(
            decision,
            GateDecision::Skip(SessionMemoryExtractionSkipReason::NoSessionId),
            "empty session_id must gate out before had_error bypass fires"
        );
    }

    /// Companion: the pathological combination — empty sid, tokens
    /// below init gate, error flag set — must also produce
    /// `NoSessionId`, not `Run` (error bypass) and not
    /// `BelowInitGate` (init gate).
    #[test]
    fn had_error_empty_sid_below_init_is_no_session_id() {
        let state = SessionMemoryState::default();
        let decision = evaluate(&state, "", 100, 0, true, false, &cfg());
        assert_eq!(
            decision,
            GateDecision::Skip(SessionMemoryExtractionSkipReason::NoSessionId)
        );
    }

    #[test]
    fn user_correction_empty_sid_below_init_is_no_session_id() {
        let state = SessionMemoryState::default();
        let decision = evaluate(&state, "", 100, 0, false, true, &cfg());
        assert_eq!(
            decision,
            GateDecision::Skip(SessionMemoryExtractionSkipReason::NoSessionId)
        );
    }
}
