//! Adaptive runtime: observation and scenario detection only.
//!
//! This module provides scenario detection and signal recording for the
//! observation plane. It does NOT mutate runtime configuration.
//!
//! Future tuning jobs (via `/tuning`) will consume these observations and
//! propose mutations through explicit write-side channels.

use std::collections::HashSet;

use super::super::agentic_loop::host::{AgenticLoopOutcome, AgenticLoopState};

fn effective_tool_metrics(state: &AgenticLoopState) -> (u32, u32) {
    if state.stall.tool_call_records.is_empty() {
        return (
            state.total_tool_calls,
            state.telemetry.all_tools_used.len() as u32,
        );
    }

    let mut unique_tools = HashSet::new();
    let mut tool_calls = 0u32;
    for record in &state.stall.tool_call_records {
        if record.is_synthetic_placeholder() {
            continue;
        }
        tool_calls += 1;
        unique_tools.insert(record.name.clone());
    }

    (tool_calls, unique_tools.len() as u32)
}

/// Decide whether the current user message implicitly accepts the previous
/// assistant turn. Acceptance fires when:
///
/// - the message is non-empty (whitespace stripped), AND
/// - it is not a direct correction, reanchor, or explicit interruption.
///
/// Acceptance is intentionally conservative: a non-empty user message is not
/// enough. If the message redirects the task or pauses the agent, recording
/// `Acceptance` would contradict the user's latest control signal.
#[must_use]
pub(crate) fn should_emit_acceptance(message: &str) -> bool {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return false;
    }
    if astra_turn_core::input_classifier::is_reanchor_signal(trimmed) {
        return false;
    }
    !is_explicit_interruption(trimmed)
}

fn is_explicit_interruption(message: &str) -> bool {
    let lower = message.to_lowercase();
    starts_with_control_word(&lower, "wait")
        || starts_with_control_word(&lower, "stop")
        || lower.contains("hold on")
        || lower.contains("等等")
        || lower.contains("先停")
}

fn starts_with_control_word(text: &str, word: &str) -> bool {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix(word) else {
        return false;
    };
    rest.chars()
        .next()
        .is_none_or(|ch| ch.is_whitespace() || matches!(ch, ',' | '.' | ':' | ';' | '!' | '?'))
}

/// Record feedback signals based on the loop's outcome and accumulated state.
///
/// Called once after the loop finishes (or errors) to feed observation and
/// SelfModel inputs.
pub(crate) fn record_loop_completion_feedback(
    state: &mut AgenticLoopState,
    result: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
) {
    use astra_core::feedback::{FeedbackSignal, SignalType};

    let hub = match &state.telemetry.observability_hub {
        Some(h) => h,
        None => return,
    };

    let turn_id = state.current_run_id.clone().unwrap_or_default();
    let session_attribution = state
        .telemetry
        .observability_session
        .as_ref()
        .map(|session| {
            let session = astra_core::sync_poison::recover_rwlock_read(session);
            crate::observability::session_signal_attribution(&session)
        });
    let enrich_signal = |signal: FeedbackSignal| {
        let mut signal =
            crate::observability::with_signal_attribution(signal, session_attribution.as_ref());
        if !signal.context.contains_key("session_id")
            && let Some(session_id) = state.current_session_id.as_deref()
        {
            signal = signal.with_context("session_id", serde_json::json!(session_id));
        }
        signal
    };

    // ── 1. Outcome signal ──
    match result {
        Ok(AgenticLoopOutcome::Completed) => {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskSuccess).with_turn(&turn_id),
            ));
        }
        Ok(AgenticLoopOutcome::Cancelled) => {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::Interruption).with_turn(&turn_id),
            ));
        }
        Ok(AgenticLoopOutcome::Error(reason)) => {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: reason.clone(),
                })
                .with_turn(&turn_id),
            ));
        }
        Err(reason) => {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: reason.to_string(),
                })
                .with_turn(&turn_id),
            ));
        }
        Ok(AgenticLoopOutcome::Waiting(_)) => {
            // No signal for waiting — the loop will resume.
        }
    }

    // ── 2. Token usage signal ──
    let total_tokens = state.provider_total_tokens();
    // Heuristic threshold: >50k tokens suggests inefficiency for most tasks.
    let token_threshold = 50_000u64;
    if total_tokens > token_threshold {
        hub.record_feedback(enrich_signal(
            FeedbackSignal::new(SignalType::HighTokenUsage {
                tokens: total_tokens,
                threshold: token_threshold,
            })
            .with_turn(&turn_id),
        ));
    }

    // ── 3. Tool churn signal ──
    let (tool_calls, unique_tools) = effective_tool_metrics(state);
    // High tool calls with low unique tools suggests repetitive/failing usage.
    if tool_calls > 10 && unique_tools > 0 && (tool_calls / unique_tools) > 5 {
        hub.record_feedback(enrich_signal(
            FeedbackSignal::new(SignalType::ToolChurn {
                calls: tool_calls,
                unique_tools,
            })
            .with_turn(&turn_id),
        ));
    }

    // ── 4. Tool-level failure signals ──
    let failed_tools: u32 = state
        .stall
        .tool_call_records
        .iter()
        .filter(|r| !r.ok)
        .count() as u32;
    if failed_tools > 0 && tool_calls > 0 {
        let failure_rate = failed_tools as f64 / tool_calls as f64;
        if failure_rate > 0.3 {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: format!(
                        "high tool failure rate: {failed_tools}/{tool_calls} ({:.0}%)",
                        failure_rate * 100.0
                    ),
                })
                .with_turn(&turn_id)
                .with_context("tool_failure_rate", serde_json::json!(failure_rate)),
            ));
        }
    }

    // ── 5. Skill quality signals ──
    for (name, entry) in state.skills.quality_tracker.all_entries() {
        if entry.invocations == 0 {
            continue;
        }
        if entry.failures > 0 {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: format!(
                        "skill '{}' failed {}/{} invocations",
                        name, entry.failures, entry.invocations
                    ),
                })
                .with_turn(&turn_id)
                .with_context("skill_name", serde_json::json!(name))
                .with_context(
                    "skill_success_rate",
                    serde_json::json!(entry.success_rate()),
                ),
            ));
        } else {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskSuccess)
                    .with_turn(&turn_id)
                    .with_context("skill_name", serde_json::json!(name))
                    .with_context("skill_invocations", serde_json::json!(entry.invocations)),
            ));
        }
    }

    // ── 6. Retry detection signal ──
    // Detect consecutive identical tool calls (same name + similar args) as retry behavior.
    {
        let records = &state.stall.tool_call_records;
        if records.len() >= 2 {
            let mut consecutive = 1u32;
            for pair in records.windows(2).rev() {
                if pair[0].name == pair[1].name
                    && pair[0].args_preview == pair[1].args_preview
                    && !pair[1].ok
                {
                    consecutive += 1;
                } else {
                    break;
                }
            }
            if consecutive >= 2 {
                hub.record_feedback(enrich_signal(
                    FeedbackSignal::new(SignalType::Retry { count: consecutive })
                        .with_turn(&turn_id)
                        .with_context(
                            "tool_name",
                            serde_json::json!(records.last().map(|r| &r.name)),
                        ),
                ));
            }
        }
    }

    // ── 7. Acceptance signal ──
    // If there is a prior assistant message and the current user message shows no correction
    // intent, emit Acceptance — the user implicitly accepted the previous output.
    {
        let has_prior_assistant = state
            .messages
            .iter()
            .rev()
            .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
        if has_prior_assistant && should_emit_acceptance(&state.message) {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::Acceptance).with_turn(&turn_id),
            ));
        }
    }

    // ── 8. Tool health signals ──
    // Emit signals for retry-cautioned tools so observation/SelfModel can react.
    {
        let retry_cautioned = state.turn_guard.health.health_avoidance_tools();
        for tool_name in retry_cautioned {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::ToolHealthAvoidance {
                    tool_name: tool_name.to_string(),
                })
                .with_turn(&turn_id),
            ));
        }
        // Track tools that were rehabilitated this session (rehab count > 0 but no active health avoidance).
        for (name, health) in state.turn_guard.health.all() {
            if health.rehabilitation_count > 0 && !health.avoidance_advised {
                hub.record_feedback(enrich_signal(
                    FeedbackSignal::new(SignalType::ToolRehabilitated {
                        tool_name: name.clone(),
                    })
                    .with_turn(&turn_id),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::should_emit_acceptance;

    #[test]
    fn acceptance_skipped_for_explicit_corrections() {
        for s in [
            "no, that's not what i meant",
            "actually, i wanted X",
            "wait, can you show the logs first?",
            "wait, hold on",
            "i meant the other one",
            "to clarify, i need Y",
            "不对，我的意思是改这里",
            "不是修修补补，要系统性解决",
            "等等，先停一下",
        ] {
            assert!(
                !should_emit_acceptance(s),
                "{s:?} is a correction; acceptance must not fire"
            );
        }
    }

    #[test]
    fn acceptance_fires_for_neutral_or_accepting_messages() {
        for s in [
            "please continue",
            "thanks, looks good",
            "next step",
            "继续",
            "ok run it",
        ] {
            assert!(should_emit_acceptance(s), "{s:?} must emit acceptance");
        }
    }

    #[test]
    fn acceptance_skipped_for_empty_or_whitespace_message() {
        assert!(!should_emit_acceptance(""));
        assert!(!should_emit_acceptance("   \n\t"));
    }

    #[test]
    fn acceptance_does_not_fire_on_drifted_inline_keyword_set() {
        assert!(should_emit_acceptance("the answer is not 5"));
        assert!(should_emit_acceptance(
            "this looks wrong-shaped, fix the layout"
        ));
    }
}
