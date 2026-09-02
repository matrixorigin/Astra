//! Turn evaluation engine — computes success/quality from turn signals.
//!
//! Replaces the crude heuristic (tool_calls > 0 → success, 0.7 quality) with
//! a multi-signal evaluator that considers:
//! - Tool error rate (fraction of calls that failed)
//! - Empty/minimal tool output
//! - Stall/verdict events from TurnGuard
//! - Budget pressure
//! - Repeated tool calls (retry loops)
//! - Correction patterns in user follow-up

use crate::orchestration::agent_result_wire::{
    AGENT_RESULT_CLASS_AGENT_INCOMPLETE as RESULT_CLASS_AGENT_INCOMPLETE,
    AGENT_RESULT_CLASS_FANOUT_INCOMPLETE as RESULT_CLASS_FANOUT_INCOMPLETE,
    agent_fanout_result_looks_like, agent_fanout_structured_result_class,
    agent_tool_result_looks_like, agent_tool_structured_result_class,
};
use astra_services::session_journal::{JournalEvent, ToolCallRecord};
use serde_json::{Value, json};

/// Signals detected during evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalSignal {
    /// Fraction of tool calls that errored (0.0 = all ok, 1.0 = all failed).
    ToolErrorRate(f64),
    /// At least one tool returned empty or minimal output (<10 bytes).
    EmptyToolOutput,
    /// TurnGuard detected a stall (name stall, intent drift, etc.).
    StallDetected,
    /// Budget pressure exceeded 0.8 (agent struggling to fit in budget).
    HighBudgetPressure,
    /// Same tool/target called 3+ times — possible retry loop.
    RepeatToolCall(String),
    /// TurnGuard issued a warning or higher verdict.
    VerdictWarning,
    /// No tools used despite likely needing them.
    NoToolsNeeded,
    /// All tools succeeded with good output.
    AllToolsHealthy,
    /// One or more tool calls returned a cache/no-op/unchanged-result stub
    /// instead of fresh observational evidence. Carries the count.
    NoOpToolResults(usize),

    /// Multiple read tool calls requested line ranges already fully covered
    /// by prior successful reads of the same unchanged file. Carries the
    /// number of redundant read events. Partial overlaps that extend into
    /// unseen content are not redundant, and an output-bounded unbounded read
    /// does not claim that the whole file reached the model.
    RedundantOverlappingReads(usize),
    /// Many search-like tool calls (grep/rg/find) were issued in one turn,
    /// suggesting the model fanned out exploratory search instead of
    /// narrowing the search space or batching more effectively. Carries the
    /// total number of search-like calls in the turn.
    SearchFanout(usize),
    /// The same heavy validation command prefix (e.g. `cargo check`,
    /// `cargo test`, `npx tsc --noEmit`, `npm test`) was retried multiple
    /// times with no intervening workspace mutation. Carries the number of
    /// redundant retries after the first run.
    RedundantValidationRetries(usize),
    /// Too many LLM rounds were consumed in a single user turn, which usually
    /// indicates tool churn or repeated replanning. Carries the round count and
    /// total prompt tokens consumed by the turn.
    LlmRoundChurn { rounds: u32, prompt_tokens: u64 },
    /// Logical prompt/input size ballooned substantially across LLM rounds in
    /// one turn. Each observed size includes fresh, cache-read, and cache-
    /// creation lanes, so a cache miss alone cannot masquerade as context
    /// growth. Carries the first observed size, the peak size, and the delta.
    PromptGrowthChurn {
        first_prompt_tokens: u64,
        max_prompt_tokens: u64,
        delta_tokens: u64,
    },
    /// Multiple consecutive *multi-call* rounds stayed inside the same
    /// exploratory tool family (diff/search/read) instead of progressing toward
    /// synthesis, mutation, or validation. Carries the dominant family and streak
    /// length.
    ExplorationFamilyChurn { family: String, streak: usize },
    /// The turn consumed materially high cost (rounds/prompt growth/budget/tool
    /// volume) while still showing low-yield exploration signals. Carries the
    /// total tool-call count and observed LLM rounds when available.
    HighCostLowYield {
        tool_calls: usize,
        llm_rounds: Option<u32>,
    },
    /// A tool command completed at the transport/execution layer but its
    /// classified outcome still represents an unresolved task failure
    /// (`test_failure`, `env_failure`, `execution_error`, or a structured
    /// orchestration incomplete class). Carries the normalized result class and
    /// number of unresolved streams.
    ToolOutcomeFailure { class: String, count: usize },
    /// Coverage for classified outcome failures across materially attempted
    /// tool calls. This distinguishes one failed optional probe in a productive
    /// review from a turn where most execution evidence is unavailable.
    ToolOutcomeFailureCoverage { unresolved: usize, observed: usize },
    /// One or more tool calls were rejected before execution by policy or
    /// runtime admission. They are not material `tools_used`, but they are
    /// still user-visible failed tool attempts and must not be evaluated as
    /// healthy execution.
    BlockedToolCall { count: usize },
}

/// Default threshold for [`EvalSignal::RedundantOverlappingReads`]: minimum
/// count of redundant read events needed before flagging the turn. Calibrated
/// against production journals: at this level the signal catches repeated
/// context re-reads while leaving healthy short turns silent.
pub const REDUNDANT_OVERLAPPING_READS_THRESHOLD: usize = 3;

/// Default threshold for [`EvalSignal::SearchFanout`]: minimum count of
/// grep/rg/find-like tool calls in a turn before flagging passive search
/// fan-out. False-positive risk is higher than redundant reads because some
/// healthy investigative turns also fan out search, so this remains
/// post-mortem only and carries a milder quality penalty.
pub const SEARCH_FANOUT_THRESHOLD: usize = 8;

/// Default threshold for [`EvalSignal::RedundantValidationRetries`]:
/// redundant retries of the SAME heavy validation prefix within a no-mutation
/// window before flagging the turn. Carries retry count (runs after the first).
/// Kept conservative because validation retries may be legitimate when there
/// was an intervening mutation or configuration change.
pub const REDUNDANT_VALIDATION_RETRIES_THRESHOLD: usize = 2;

/// Default threshold for [`EvalSignal::LlmRoundChurn`]. Round count is a cost
/// fact, not a failure fact: the signal is emitted only when an independent
/// typed low-yield observation also proves that a long turn is not progressing.
pub const LLM_ROUND_CHURN_THRESHOLD: usize = 8;
pub const EXPLORATION_FAMILY_CHURN_THRESHOLD: usize = 3;
pub const ONLINE_PROGRESS_MIN_TOOL_CALLS_BEFORE_NUDGE: usize = 2;
pub const PROMPT_GROWTH_CHURN_MIN_ROUNDS: u32 = 4;
pub const PROMPT_GROWTH_CHURN_MIN_DELTA_TOKENS: u64 = 8_000;
pub const PROMPT_GROWTH_CHURN_MIN_RATIO_NUMERATOR: u64 = 2;
pub const PROMPT_GROWTH_CHURN_MIN_RATIO_DENOMINATOR: u64 = 1;
const HIGH_COST_TOOL_CALL_THRESHOLD: usize = 16;
/// Post-mortem evaluation thresholds. Defaults mirror the calibrated compile-
/// time constants above, but runtime callers may override them from config so
/// passive eval signals can be tuned without a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationThresholds {
    pub redundant_overlapping_reads: usize,
    pub search_fanout: usize,
    pub redundant_validation_retries: usize,
    pub llm_round_churn: usize,
    pub exploration_family_churn: usize,
}

impl Default for EvaluationThresholds {
    fn default() -> Self {
        Self {
            redundant_overlapping_reads: REDUNDANT_OVERLAPPING_READS_THRESHOLD,
            search_fanout: SEARCH_FANOUT_THRESHOLD,
            redundant_validation_retries: REDUNDANT_VALIDATION_RETRIES_THRESHOLD,
            llm_round_churn: LLM_ROUND_CHURN_THRESHOLD,
            exploration_family_churn: EXPLORATION_FAMILY_CHURN_THRESHOLD,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TurnEvaluationTelemetry {
    pub llm_rounds: Option<u32>,
    pub prompt_tokens: Option<u64>,
    pub first_round_prompt_tokens: Option<u64>,
    pub max_round_prompt_tokens: Option<u64>,
}

/// Conservative mid-loop progress policy.
///
/// This is intentionally narrower than post-mortem turn evaluation: it does
/// not infer task intent from user prose, does not parse tool output text, and
/// never hard-stops a turn. It only recommends low-risk advisory evidence when
/// structured observations show repeated low-yield work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnlineProgressPolicy {
    pub redundant_overlapping_reads_threshold: usize,
    pub min_tool_calls_before_nudge: usize,
}

impl Default for OnlineProgressPolicy {
    fn default() -> Self {
        Self {
            redundant_overlapping_reads_threshold: REDUNDANT_OVERLAPPING_READS_THRESHOLD,
            min_tool_calls_before_nudge: ONLINE_PROGRESS_MIN_TOOL_CALLS_BEFORE_NUDGE,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OnlineProgressSignals {
    pub tool_calls: usize,
    pub redundant_overlapping_reads: usize,
    pub stronger_advisory_emitted: bool,
    pub advisory_already_emitted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnlineProgressDecision {
    Continue,
    ReuseKnownContext { redundant_overlapping_reads: usize },
}

impl OnlineProgressDecision {
    const fn continue_turn() -> Self {
        Self::Continue
    }
}

pub fn decide_online_progress(
    signals: OnlineProgressSignals,
    policy: OnlineProgressPolicy,
) -> OnlineProgressDecision {
    if signals.stronger_advisory_emitted || signals.advisory_already_emitted {
        return OnlineProgressDecision::continue_turn();
    }

    if signals.tool_calls < policy.min_tool_calls_before_nudge {
        return OnlineProgressDecision::continue_turn();
    }

    if threshold_reached(
        signals.redundant_overlapping_reads,
        policy.redundant_overlapping_reads_threshold,
    ) {
        return OnlineProgressDecision::ReuseKnownContext {
            redundant_overlapping_reads: signals.redundant_overlapping_reads,
        };
    }

    OnlineProgressDecision::continue_turn()
}

const fn threshold_reached(count: usize, threshold: usize) -> bool {
    threshold > 0 && count >= threshold
}

/// Result of evaluating a turn's success and quality.
#[derive(Debug, Clone)]
pub struct TurnEvaluation {
    /// Whether the turn is considered successful.
    pub success: bool,
    /// Quality score 0.0–1.0.
    pub quality: f64,
    /// How confident we are in this evaluation (0.0–1.0).
    pub confidence: f64,
    /// Signals that contributed to the evaluation.
    pub signals: Vec<EvalSignal>,
    /// Thresholds used when deriving configurable passive eval signals.
    pub thresholds: EvaluationThresholds,
}

pub fn turn_evaluation_status_notice(eval: &TurnEvaluation) -> Option<String> {
    let outcome_failures = eval
        .signals
        .iter()
        .filter_map(|signal| match signal {
            EvalSignal::ToolOutcomeFailure { class, count } => Some(format!("{class} x{count}")),
            EvalSignal::BlockedToolCall { count } => Some(format!("blocked_tool x{count}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !outcome_failures.is_empty() {
        return Some(format!(
            "Turn finished with unresolved tool/runtime failure(s): {}. Treat the final answer as incomplete until validation passes or the provider surface changes.",
            outcome_failures.join(", ")
        ));
    }

    if eval.success {
        return None;
    }

    let reason = eval
        .signals
        .iter()
        .find_map(turn_evaluation_signal_reason)
        .unwrap_or("turn evaluation failed");
    Some(format!(
        "Turn evaluation marked this turn incomplete (quality {:.2}): {reason}.",
        eval.quality
    ))
}

/// Whether the turn contains unresolved execution evidence for diagnostics.
///
/// This is narrower than `!eval.success`: conversational turns and low-quality
/// answers may score unsuccessfully without having an execution failure. This
/// is an outcome-quality signal, not lifecycle authority: callers must never
/// rewrite an explicitly completed/cancelled/failed run from this heuristic.
pub fn turn_evaluation_has_unresolved_execution_failure(eval: &TurnEvaluation) -> bool {
    let has_nested_incomplete_run = eval.signals.iter().any(|signal| {
        matches!(
            signal,
            EvalSignal::ToolOutcomeFailure { class, .. }
                if class == RESULT_CLASS_AGENT_INCOMPLETE
                    || class == RESULT_CLASS_FANOUT_INCOMPLETE
        )
    });
    if has_nested_incomplete_run {
        return true;
    }

    let has_execution_failure = eval.signals.iter().any(|signal| {
        matches!(
            signal,
            EvalSignal::ToolOutcomeFailure { .. } | EvalSignal::BlockedToolCall { .. }
        )
    });
    if !has_execution_failure {
        return false;
    }

    // A minority failure in an otherwise productive turn remains visible as
    // evaluation evidence without classifying the overall execution evidence
    // as unresolved. Review agents commonly use optional probes; one
    // unavailable probe among many successful reads does not erase the useful
    // evidence they returned.
    // A majority failure or even split means diagnostic evidence remains
    // unresolved. Evaluations without a rate are treated conservatively because
    // hand-built/external evaluations cannot prove successful recovery. This
    // threshold affects outcome diagnostics only, never run lifecycle.
    if let Some(is_majority) = eval.signals.iter().find_map(|signal| match signal {
        EvalSignal::ToolOutcomeFailureCoverage {
            unresolved,
            observed,
        } => Some(*observed == 0 || unresolved.saturating_mul(2) >= *observed),
        _ => None,
    }) {
        return is_majority;
    }

    eval.signals
        .iter()
        .find_map(|signal| match signal {
            EvalSignal::ToolErrorRate(rate) => Some(*rate >= 0.5),
            _ => None,
        })
        .unwrap_or(true)
}

pub fn turn_evaluation_signal_reason(signal: &EvalSignal) -> Option<&'static str> {
    match signal {
        EvalSignal::ToolErrorRate(rate) if *rate >= 0.5 => Some("tool error rate is high"),
        EvalSignal::StallDetected => Some("stall/divergence was detected"),
        EvalSignal::VerdictWarning => Some("TurnGuard emitted a warning"),
        EvalSignal::NoToolsNeeded => Some("needed tools were not used"),
        EvalSignal::BlockedToolCall { .. } => Some("tool calls were blocked before execution"),
        EvalSignal::HighCostLowYield { .. } => Some("high-cost exploration produced low yield"),
        EvalSignal::LlmRoundChurn { .. } => Some("too many LLM rounds were used"),
        EvalSignal::PromptGrowthChurn { .. } => Some("prompt size ballooned across rounds"),
        EvalSignal::RedundantValidationRetries(_) => Some("validation was retried redundantly"),
        EvalSignal::RedundantOverlappingReads(_) => Some("content was re-read redundantly"),
        EvalSignal::ExplorationFamilyChurn { .. } => Some("exploration stayed in one tool family"),
        _ => None,
    }
}

/// Per-tool-call record for evaluation (matches ToolCallRecord shape).
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub name: String,
    pub repeat_key: String,
    pub ok: bool,
    pub ms: u64,
    pub error: Option<String>,
    pub output_bytes: Option<u32>,
    pub no_op: bool,
}

/// Evaluate a completed turn from its observable signals.
///
/// # Arguments
/// - `tool_calls` — per-tool-call audit records
/// - `stall_count` — number of stall events in this turn
/// - `verdict_warning` — whether TurnGuard issued a Warning or higher
/// - `budget_pressure` — 0.0–0.9 budget pressure from compaction tier
/// - `is_factual_query` — whether the query likely needed tool calls
pub fn evaluate_turn(
    tool_calls: &[ToolCallInfo],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    is_factual_query: bool,
) -> TurnEvaluation {
    let mut signals = Vec::new();
    let mut quality = 0.5_f64; // base quality
    let mut confidence = 0.5_f64; // base confidence

    let total_calls = tool_calls.len();

    // ─── No tool calls ──────────────────────────────────────────────────
    if total_calls == 0 {
        if is_factual_query {
            // Needed tools but didn't use any — bad
            signals.push(EvalSignal::NoToolsNeeded);
            return TurnEvaluation {
                success: false,
                quality: 0.2,
                confidence: 0.7,
                signals,
                thresholds: EvaluationThresholds::default(),
            };
        }
        // Conversational turn — no tools expected
        return TurnEvaluation {
            success: true,
            quality: 0.5,
            confidence: 0.4, // low confidence for text-only turns
            signals,
            thresholds: EvaluationThresholds::default(),
        };
    }

    // ─── Tool error analysis ────────────────────────────────────────────
    let error_count = tool_calls.iter().filter(|tc| !tc.ok).count();
    let error_rate = error_count as f64 / total_calls as f64;
    let no_op_count = tool_calls.iter().filter(|tc| tc.no_op).count();
    signals.push(EvalSignal::ToolErrorRate(error_rate));

    if no_op_count > 0 {
        signals.push(EvalSignal::NoOpToolResults(no_op_count));
        let penalty = (0.08 * (no_op_count as f64 / total_calls as f64)).clamp(0.03, 0.15);
        quality -= penalty;
    }

    if error_rate == 0.0 && no_op_count < total_calls {
        // All tools succeeded
        quality += 0.3;
        confidence += 0.2;
        signals.push(EvalSignal::AllToolsHealthy);
    } else if error_rate == 0.0 {
        // All reported ok, but none produced fresh evidence.
        confidence += 0.05;
    } else if error_rate < 0.5 {
        // Partial failures can still leave enough evidence for the model to
        // answer, but the turn is not operationally successful. Keep the
        // quality signal moderate while making the final success verdict
        // depend on zero failed tools below.
        quality -= 0.05;
        confidence += 0.1;
    } else {
        // Majority errors
        quality -= 0.2;
        confidence += 0.1; // more confident this is bad
    }

    // ─── Empty output detection ─────────────────────────────────────────
    let empty_outputs = tool_calls
        .iter()
        .filter(|tc| tc.ok && tc.output_bytes.is_some_and(|b| b < 10))
        .count();
    if empty_outputs > 0 {
        signals.push(EvalSignal::EmptyToolOutput);
        quality -= 0.1 * (empty_outputs as f64 / total_calls as f64);
    }

    // ─── Repeat tool detection (retry loops) ────────────────────────────
    let mut call_counts: std::collections::HashMap<&str, (&str, usize)> =
        std::collections::HashMap::new();
    for tc in tool_calls {
        let entry = call_counts
            .entry(tc.repeat_key.as_str())
            .or_insert((tc.name.as_str(), 0));
        entry.1 += 1;
    }
    for (name, count) in call_counts.values() {
        if *count >= 3 {
            signals.push(EvalSignal::RepeatToolCall((*name).to_string()));
            quality -= 0.15;
        }
    }

    // ─── Stall events ───────────────────────────────────────────────────
    if stall_count > 0 {
        signals.push(EvalSignal::StallDetected);
        quality -= 0.1 * stall_count.min(3) as f64;
        confidence += 0.1;
    }

    // ─── TurnGuard verdict ──────────────────────────────────────────────
    if verdict_warning {
        signals.push(EvalSignal::VerdictWarning);
        quality -= 0.15;
        confidence += 0.1;
    }

    // ─── Budget pressure ────────────────────────────────────────────────
    if budget_pressure > 0.8 {
        signals.push(EvalSignal::HighBudgetPressure);
        quality -= 0.05;
    }

    // ─── Determine success ──────────────────────────────────────────────
    let success = error_count == 0 && quality > 0.3;

    TurnEvaluation {
        success,
        quality: quality.clamp(0.0, 1.0),
        confidence: confidence.clamp(0.0, 1.0),
        signals,
        thresholds: EvaluationThresholds::default(),
    }
}

pub fn evaluate_tool_call_records(
    input: &str,
    recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
) -> TurnEvaluation {
    evaluate_tool_call_records_with_thresholds_and_telemetry(
        input,
        recent_tools,
        tool_call_records,
        stall_count,
        verdict_warning,
        budget_pressure,
        EvaluationThresholds::default(),
        TurnEvaluationTelemetry::default(),
    )
}

pub fn evaluate_tool_call_records_with_thresholds(
    input: &str,
    recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    thresholds: EvaluationThresholds,
) -> TurnEvaluation {
    evaluate_tool_call_records_with_thresholds_and_telemetry(
        input,
        recent_tools,
        tool_call_records,
        stall_count,
        verdict_warning,
        budget_pressure,
        thresholds,
        TurnEvaluationTelemetry::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_tool_call_records_with_thresholds_and_telemetry(
    _input: &str,
    _recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    thresholds: EvaluationThresholds,
    telemetry: TurnEvaluationTelemetry,
) -> TurnEvaluation {
    // Execution health is computed only from calls that reached an executor.
    // Rejected, reused, suppressed, and deferred requests remain available as
    // typed audit evidence but must not be mislabeled as execution failures.
    let tool_calls = tool_call_records
        .iter()
        .filter(|record| record_contributes_tool_evidence(record))
        .map(|record| ToolCallInfo {
            name: record.name.clone(),
            // Prefer the *untruncated* args for the repeat-key. `args_preview`
            // is capped at ~80 chars, so two distinct calls that share a long
            // common prefix (e.g. `grep -n '<long-pattern>' /workspace/astr…`)
            // collide and surface as a false repeat-loop. Hash `args_full`
            // when present to keep the key bounded; fall back to the preview
            // for legacy records, then to the bare tool name.
            repeat_key: record
                .authoritative_args_full()
                .map(str::trim)
                .filter(|full| !full.is_empty())
                .map(|full| {
                    // Hash is used only for in-process dedup within a single
                    // `evaluate_tool_call_records` call — never persisted or
                    // compared across Rust versions. DefaultHasher is fine here.
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    full.hash(&mut hasher);
                    format!("{}::full::{:016x}", record.name, hasher.finish())
                })
                .or_else(|| {
                    record
                        .args_preview
                        .as_deref()
                        .map(str::trim)
                        .filter(|preview| !preview.is_empty())
                        .map(|preview| format!("{}::preview::{preview}", record.name))
                })
                .unwrap_or_else(|| record.name.clone()),
            ok: record.ok,
            ms: record.ms,
            error: record.error.clone(),
            output_bytes: record.output_bytes,
            // A validated cache hit re-delivers the requested evidence.
            // Suppression only points at evidence already delivered in this
            // model boundary, so it remains a no-op for progress evaluation.
            no_op: record_is_suppressed_noop(record),
        })
        .collect::<Vec<_>>();
    let is_live_query = records_include_live_query(tool_call_records);
    let mut eval = evaluate_turn(
        &tool_calls,
        stall_count,
        verdict_warning,
        budget_pressure,
        is_live_query,
    );
    eval.thresholds = thresholds;

    // ─── Redundant overlapping read detection ───────────────────────────
    // Detects the failure mode where the model re-reads the SAME file/line
    // range multiple times across a turn instead of referring back to its
    // own prior tool outputs. The detector excludes the legitimate
    // "mutate then re-read to verify" pattern: any workspace-mutating tool
    // call clears the per-file read history, so a re-read AFTER an edit
    // does NOT contribute to the count. Strictly observational at this
    // tier — no mid-loop intervention.
    let redundant_reads = count_redundant_overlapping_reads(tool_call_records);
    if redundant_reads >= thresholds.redundant_overlapping_reads {
        eval.signals
            .push(EvalSignal::RedundantOverlappingReads(redundant_reads));
        let penalty = (0.03
            + 0.01 * (redundant_reads - thresholds.redundant_overlapping_reads) as f64)
            .clamp(0.03, 0.15);
        eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
    }

    // ─── Search fan-out detection ────────────────────────────────────────
    // Detects turns that spray many grep/rg/find-like searches instead of
    // narrowing the search space or switching to more direct reads. This
    // signal is intentionally mild and observational only: some healthy
    // investigations legitimately use many searches, so we do NOT intervene
    // at runtime based on it without more calibration.
    let search_fanout = count_search_fanout(tool_call_records);
    if search_fanout >= thresholds.search_fanout {
        eval.signals.push(EvalSignal::SearchFanout(search_fanout));
        let penalty =
            (0.02 + 0.005 * (search_fanout - thresholds.search_fanout) as f64).clamp(0.02, 0.12);
        eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
    }

    // Detect redundant retries of the same heavy validation prefix in a
    // no-mutation window (e.g. `cargo check`, `cargo test`, `npx tsc --noEmit`).
    // This is narrower and higher-precision than generic RepeatToolCall because
    // it collapses harmless output-shaping suffixes like `| head -40`,
    // `| tail -20`, and `&& grep ...`.
    let redundant_validation_retries = max_redundant_validation_retries(tool_call_records);
    if redundant_validation_retries >= thresholds.redundant_validation_retries {
        eval.signals.push(EvalSignal::RedundantValidationRetries(
            redundant_validation_retries,
        ));
        let penalty = (0.03
            + 0.01
                * (redundant_validation_retries - thresholds.redundant_validation_retries) as f64)
            .clamp(0.03, 0.10);
        eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
    }

    if let (Some(rounds), Some(first_prompt_tokens), Some(max_round_prompt_tokens)) = (
        telemetry.llm_rounds,
        telemetry.first_round_prompt_tokens,
        telemetry.max_round_prompt_tokens,
    ) {
        let delta_tokens = max_round_prompt_tokens.saturating_sub(first_prompt_tokens);
        let prompt_doubled = first_prompt_tokens > 0
            && max_round_prompt_tokens.saturating_mul(PROMPT_GROWTH_CHURN_MIN_RATIO_DENOMINATOR)
                >= first_prompt_tokens.saturating_mul(PROMPT_GROWTH_CHURN_MIN_RATIO_NUMERATOR);
        if rounds >= PROMPT_GROWTH_CHURN_MIN_ROUNDS
            && delta_tokens >= PROMPT_GROWTH_CHURN_MIN_DELTA_TOKENS
            && prompt_doubled
        {
            eval.signals.push(EvalSignal::PromptGrowthChurn {
                first_prompt_tokens,
                max_prompt_tokens: max_round_prompt_tokens,
                delta_tokens,
            });
            eval.quality = (eval.quality - 0.08).clamp(0.0, 1.0);
            eval.confidence = (eval.confidence + 0.05).clamp(0.0, 1.0);
        }
    }

    if let Some((family, streak)) = longest_exploration_family_round_streak(tool_call_records)
        .filter(|(_, streak)| *streak >= thresholds.exploration_family_churn)
    {
        eval.signals.push(EvalSignal::ExplorationFamilyChurn {
            family: family.as_str().to_string(),
            streak,
        });
        let penalty =
            (0.04 + 0.01 * (streak - thresholds.exploration_family_churn) as f64).clamp(0.04, 0.16);
        eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
    }

    // Astra deliberately supports long, iterative work. A high round count
    // must never become a quality verdict by itself; require an independent,
    // typed low-yield fact such as repeated reads/validation or an exploration
    // streak. This mirrors RuntimePolicy's online rule and avoids teaching the
    // agent to stop merely because useful work takes time.
    if eval.signals.iter().any(is_strong_low_yield_signal)
        && let Some(rounds) = telemetry
            .llm_rounds
            .filter(|rounds| *rounds as usize >= thresholds.llm_round_churn)
    {
        let prompt_tokens = telemetry.prompt_tokens.unwrap_or(0);
        eval.signals.push(EvalSignal::LlmRoundChurn {
            rounds,
            prompt_tokens,
        });
        let penalty =
            (0.10 + 0.02 * (rounds as usize - thresholds.llm_round_churn) as f64).clamp(0.10, 0.25);
        eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
        eval.confidence = (eval.confidence + 0.05).clamp(0.0, 1.0);
    }

    revoke_all_tools_healthy_when_quality_signals_disagree(&mut eval, &tool_calls);
    align_high_cost_low_yield_verdict(&mut eval, &tool_calls, telemetry);
    apply_blocked_tool_failures(&mut eval, tool_call_records);
    apply_unresolved_tool_outcome_failures(&mut eval, tool_call_records);
    calibrate_confidence_after_quality_penalties(&mut eval);

    eval
}

fn record_is_suppressed_noop(record: &ToolCallRecord) -> bool {
    record.ok
        && record.effective_disposition()
            == astra_services::session_journal::ToolCallDisposition::Suppressed
        && record_is_model_visible_result(record)
}

fn record_is_validated_reuse(record: &ToolCallRecord) -> bool {
    record.ok
        && record.effective_disposition()
            == astra_services::session_journal::ToolCallDisposition::Reused
        && record_is_model_visible_result(record)
}

fn record_is_model_visible_result(record: &ToolCallRecord) -> bool {
    // Skill routing and surgical-removal records are audit placeholders, not
    // evidence delivered at the model boundary.
    record.surgically_removed != Some(true)
        && record.skill_reentry_count.is_none()
        && record.skill_locked_out != Some(true)
}

fn record_contributes_tool_evidence(record: &ToolCallRecord) -> bool {
    record_was_executed(record)
        || record_is_validated_reuse(record)
        || record_is_suppressed_noop(record)
}

fn record_was_executed(record: &ToolCallRecord) -> bool {
    record.effective_disposition() == astra_services::session_journal::ToolCallDisposition::Executed
}

fn is_negative_quality_signal(signal: &EvalSignal) -> bool {
    matches!(
        signal,
        EvalSignal::EmptyToolOutput
            | EvalSignal::StallDetected
            | EvalSignal::HighBudgetPressure
            | EvalSignal::RepeatToolCall(_)
            | EvalSignal::VerdictWarning
            | EvalSignal::NoOpToolResults(_)
            | EvalSignal::RedundantOverlappingReads(_)
            | EvalSignal::SearchFanout(_)
            | EvalSignal::RedundantValidationRetries(_)
            | EvalSignal::LlmRoundChurn { .. }
            | EvalSignal::PromptGrowthChurn { .. }
            | EvalSignal::ExplorationFamilyChurn { .. }
            | EvalSignal::HighCostLowYield { .. }
            | EvalSignal::ToolOutcomeFailure { .. }
            | EvalSignal::BlockedToolCall { .. }
    )
}

fn is_high_cost_signal(signal: &EvalSignal) -> bool {
    matches!(
        signal,
        EvalSignal::HighBudgetPressure
            | EvalSignal::LlmRoundChurn { .. }
            | EvalSignal::PromptGrowthChurn { .. }
    )
}

fn is_strong_low_yield_signal(signal: &EvalSignal) -> bool {
    matches!(
        signal,
        EvalSignal::RepeatToolCall(_)
            | EvalSignal::NoOpToolResults(_)
            | EvalSignal::RedundantOverlappingReads(_)
            | EvalSignal::RedundantValidationRetries(_)
            | EvalSignal::ExplorationFamilyChurn { .. }
    )
}

fn is_low_yield_signal(signal: &EvalSignal) -> bool {
    matches!(
        signal,
        EvalSignal::EmptyToolOutput
            | EvalSignal::RepeatToolCall(_)
            | EvalSignal::NoOpToolResults(_)
            | EvalSignal::RedundantOverlappingReads(_)
            | EvalSignal::SearchFanout(_)
            | EvalSignal::RedundantValidationRetries(_)
            | EvalSignal::ExplorationFamilyChurn { .. }
    )
}

fn revoke_all_tools_healthy_when_quality_signals_disagree(
    eval: &mut TurnEvaluation,
    tool_calls: &[ToolCallInfo],
) {
    if !eval
        .signals
        .iter()
        .any(|signal| matches!(signal, EvalSignal::AllToolsHealthy))
        || !eval.signals.iter().any(is_negative_quality_signal)
    {
        return;
    }

    eval.signals
        .retain(|signal| !matches!(signal, EvalSignal::AllToolsHealthy));
    eval.quality = (eval.quality - 0.15).clamp(0.0, 1.0);
    eval.confidence = (eval.confidence + 0.05).clamp(0.0, 1.0);

    let error_count = tool_calls.iter().filter(|tc| !tc.ok).count();
    let error_rate = error_count as f64 / tool_calls.len().max(1) as f64;
    eval.success = error_rate < 0.5 && eval.quality > 0.3;
}

fn align_high_cost_low_yield_verdict(
    eval: &mut TurnEvaluation,
    tool_calls: &[ToolCallInfo],
    telemetry: TurnEvaluationTelemetry,
) {
    if eval
        .signals
        .iter()
        .any(|signal| matches!(signal, EvalSignal::HighCostLowYield { .. }))
    {
        return;
    }

    let has_high_cost = tool_calls.len() >= HIGH_COST_TOOL_CALL_THRESHOLD
        || eval.signals.iter().any(is_high_cost_signal);
    let low_yield_count = eval
        .signals
        .iter()
        .filter(|signal| is_low_yield_signal(signal))
        .count();
    let has_strong_low_yield = eval.signals.iter().any(is_strong_low_yield_signal);

    if !has_high_cost || !(has_strong_low_yield || low_yield_count >= 2) {
        return;
    }

    eval.signals.push(EvalSignal::HighCostLowYield {
        tool_calls: tool_calls.len(),
        llm_rounds: telemetry.llm_rounds,
    });
    eval.quality = (eval.quality - 0.12).clamp(0.0, 1.0);
    eval.confidence = (eval.confidence - 0.25)
        .clamp(0.0, 1.0)
        .min(high_cost_low_yield_confidence_cap(eval.quality));

    let error_count = tool_calls.iter().filter(|tc| !tc.ok).count();
    let error_rate = error_count as f64 / tool_calls.len().max(1) as f64;
    eval.success = error_rate < 0.5 && eval.quality > 0.35;
}

fn high_cost_low_yield_confidence_cap(quality: f64) -> f64 {
    (0.25 + quality.clamp(0.0, 1.0) * 0.60).clamp(0.25, 0.55)
}

fn calibrate_confidence_after_quality_penalties(eval: &mut TurnEvaluation) {
    if eval
        .signals
        .iter()
        .any(|signal| matches!(signal, EvalSignal::HighCostLowYield { .. }))
    {
        eval.confidence = eval
            .confidence
            .min(high_cost_low_yield_confidence_cap(eval.quality));
    }
}

fn result_class_is_outcome_failure(class: &str) -> bool {
    matches!(
        class,
        "test_failure"
            | "env_failure"
            | "execution_error"
            | RESULT_CLASS_AGENT_INCOMPLETE
            | RESULT_CLASS_FANOUT_INCOMPLETE
    )
}

fn result_class_resolves_outcome_failure(class: &str) -> bool {
    matches!(class, "success" | "domain_negative")
}

/// Whether a non-zero process result is explicitly a domain outcome rather
/// than an execution failure. `ToolCallRecord::ok` is intentionally not the
/// sole source of truth: grep no-match, diff differences, and truncated
/// pipelines can be valid observations with a non-zero exit status. An
/// explicit typed failure class still wins over the process-level semantic.
fn record_is_non_failure_outcome(record: &ToolCallRecord) -> bool {
    if let Some(class) = effective_tool_result_class(record) {
        if result_class_is_outcome_failure(&class) {
            return false;
        }
        if result_class_resolves_outcome_failure(&class) {
            return true;
        }
    }
    if record.ok {
        return false;
    }
    matches!(
        record.exit_semantics.as_deref().and_then(|tag| {
            serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(
                serde_json::Value::String(tag.to_string()),
            )
            .ok()
        }),
        Some(
            astra_tools::exit_semantics::ExitSemantics::Success
                | astra_tools::exit_semantics::ExitSemantics::EmptyResult
                | astra_tools::exit_semantics::ExitSemantics::DomainNegative
                | astra_tools::exit_semantics::ExitSemantics::PipelineTruncated
        )
    )
}

fn hash_bounded_key(value: &str) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn outcome_resolution_key(record: &ToolCallRecord) -> Option<String> {
    let class = effective_tool_result_class(record)?;
    if !result_class_is_outcome_failure(&class) && !result_class_resolves_outcome_failure(&class) {
        return None;
    }

    operation_identity_key(record)
}

fn fallback_tool_outcome_identity(record: &ToolCallRecord) -> Option<String> {
    operation_identity_key(record)
}

/// Stable identity of the governed operation, independent of result class,
/// disposition, provider attempt id, or JSON object key order. Outcome
/// classifiers are ledger values; they must never change the operation key
/// used to correlate a failure with a later retry.
fn operation_identity_key(record: &ToolCallRecord) -> Option<String> {
    let args = record.authoritative_args_full().unwrap_or("").trim();
    if !args.is_empty() {
        if let Some(prefix) = normalize_validation_prefix(&record.name, args) {
            return Some(format!("validation::{prefix}"));
        }
        return Some(format!(
            "tool::{}::args::{}",
            record.name,
            hash_bounded_key(&crate::stall::canonical_tool_args(args))
        ));
    }

    let preview = record.args_preview.as_deref().unwrap_or("").trim();
    if !preview.is_empty() {
        return Some(format!(
            "tool::{}::preview::{}",
            record.name,
            hash_bounded_key(&crate::stall::canonical_tool_args(preview))
        ));
    }

    record
        .tool_call_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| format!("tool::{}::call::{}", record.name, hash_bounded_key(id)))
}

fn unresolved_tool_outcome_failure_counts(
    records: &[ToolCallRecord],
) -> std::collections::BTreeMap<String, usize> {
    let mut unresolved_by_key = std::collections::BTreeMap::<String, String>::new();

    for (record_index, record) in records.iter().enumerate() {
        let disposition = record.effective_disposition();
        if !matches!(
            disposition,
            astra_services::session_journal::ToolCallDisposition::Executed
                | astra_services::session_journal::ToolCallDisposition::Rejected
        ) {
            continue;
        }

        // A rejected domain carrier never began that domain execution. Its
        // rendered payload can still look like `agent_incomplete`; do not
        // charge the request once as blocked and again as unfinished work.
        // Preserve the one rejected class that is itself execution evidence:
        // `execution_error` means route/launch setup was attempted and failed
        // before the executor could report an Executed disposition.
        if disposition == astra_services::session_journal::ToolCallDisposition::Rejected
            && effective_tool_result_class(record).as_deref() != Some("execution_error")
        {
            continue;
        }

        // A governed execution may fail before it can attach a structured
        // result class (for example a process launch, transport, or sandbox
        // failure). `ok=false` is already the typed terminal fact at this
        // boundary; do not silently lose it just because the optional
        // classifier was unavailable. Rejected requests remain a separate
        // signal and must not be double-counted as unresolved execution.
        if disposition == astra_services::session_journal::ToolCallDisposition::Executed
            && !record.ok
            && !record_is_non_failure_outcome(record)
        {
            let key = outcome_resolution_key(record).unwrap_or_else(|| {
                fallback_tool_outcome_identity(record).unwrap_or_else(|| {
                    // Without structured class, arguments, preview, or a
                    // provider call id, there is no safe success-to-failure
                    // identity. Keep the failure visible under a unique
                    // opaque key instead of allowing another same-named call
                    // to erase it.
                    format!("tool::{}::opaque::{record_index}", record.name)
                })
            });
            let class =
                effective_tool_result_class(record).unwrap_or_else(|| "tool_failure".to_string());
            unresolved_by_key.insert(key, class);
            continue;
        }

        // A successful unclassified execution resolves an earlier fallback
        // failure for the same governed operation. Typed classes below still
        // provide the more precise resolution path when available.
        if record.ok {
            if let Some(key) = fallback_tool_outcome_identity(record) {
                unresolved_by_key.remove(&key);
            }
            // A successful exact operation resolves a typed failure even when
            // the success record omits an optional result class. This keeps
            // the recovery contract based on the canonical operation identity
            // rather than requiring every executor path to repeat metadata.
            if let Some(key) = untyped_operation_key(record) {
                unresolved_by_key.remove(&key);
            }
        }
        let Some(class) = effective_tool_result_class(record) else {
            continue;
        };
        let Some(key) = outcome_resolution_key(record) else {
            continue;
        };

        if result_class_is_outcome_failure(&class) {
            unresolved_by_key.insert(key, class);
        } else if result_class_resolves_outcome_failure(&class) {
            unresolved_by_key.remove(&key);
        }
    }

    let mut counts = std::collections::BTreeMap::new();
    for class in unresolved_by_key.values() {
        *counts.entry(class.clone()).or_insert(0) += 1;
    }
    counts
}

/// Return the canonical operation identity used by the terminal outcome
/// ledger for a record.  Runtime policy and terminal evaluation must share
/// this exact identity; callers should not derive a second key from tool names
/// or ad-hoc argument fields.
pub fn tool_outcome_operation_key(record: &ToolCallRecord) -> Option<String> {
    outcome_resolution_key(record).or_else(|| {
        if record.ok {
            untyped_operation_key(record)
        } else {
            fallback_tool_outcome_identity(record)
        }
    })
}

/// All canonical identities a successful execution may use to resolve an
/// earlier failure. The list is intentionally produced by the same core
/// ledger, including typed, untyped-argument, and fallback identities.
pub fn tool_outcome_recovery_keys(record: &ToolCallRecord) -> Vec<String> {
    let mut keys = Vec::with_capacity(3);
    for key in [
        tool_outcome_operation_key(record),
        fallback_tool_outcome_identity(record),
        untyped_operation_key(record),
    ]
    .into_iter()
    .flatten()
    {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Return currently active failed execution obligations using the same ledger
/// semantics as [`count_unresolved_tool_outcome_failures`]. Records without a
/// stable identity remain countable by terminal evaluation, but are omitted
/// from this set because a later success cannot safely prove that it resolved
/// that opaque failure.
pub fn active_execution_failure_operation_keys(
    records: &[ToolCallRecord],
) -> std::collections::BTreeSet<String> {
    let mut unresolved = std::collections::BTreeMap::<String, String>::new();

    for record in records {
        if record.effective_disposition()
            != astra_services::session_journal::ToolCallDisposition::Executed
        {
            continue;
        }
        if !record.ok && !record_is_non_failure_outcome(record) {
            if let Some(key) = tool_outcome_operation_key(record) {
                let class = effective_tool_result_class(record)
                    .unwrap_or_else(|| "tool_failure".to_string());
                unresolved.insert(key, class);
            }
            continue;
        }
        if let Some(key) = fallback_tool_outcome_identity(record) {
            unresolved.remove(&key);
        }
        if let Some(key) = untyped_operation_key(record) {
            unresolved.remove(&key);
        }
        if let Some(key) = outcome_resolution_key(record)
            && result_class_resolves_outcome_failure(
                effective_tool_result_class(record)
                    .as_deref()
                    .unwrap_or_default(),
            )
        {
            unresolved.remove(&key);
        }
    }

    unresolved.into_keys().collect()
}

fn untyped_operation_key(record: &ToolCallRecord) -> Option<String> {
    operation_identity_key(record)
}

/// Return currently active rejected-operation obligations using the same
/// exact-argument resolution key as terminal evaluation.
pub fn active_rejected_operation_keys(
    records: &[ToolCallRecord],
) -> std::collections::BTreeSet<String> {
    let mut unresolved = std::collections::BTreeSet::new();
    for (record_index, record) in records.iter().enumerate() {
        let key = rejected_operation_key(record)
            .unwrap_or_else(|| blocked_resolution_key(record, record_index));
        if record_is_rejected_attempt(record) {
            unresolved.insert(key);
        } else if record_was_executed(record) && record.ok {
            unresolved.remove(&key);
        }
    }
    unresolved
}

/// Stable identity for a rejected request when its arguments or provider call
/// id make a later exact execution matchable. Opaque rejected requests remain
/// terminal evidence but cannot be safely used for cause-level persistence.
pub fn rejected_operation_key(record: &ToolCallRecord) -> Option<String> {
    operation_identity_key(record)
}

/// Number of currently unresolved, typed tool outcomes.
///
/// This is the online-safe projection of the same failure ledger used by
/// terminal turn evaluation. A later authoritative success/domain-negative
/// result for the same operation removes the earlier failure, so runtime
/// feedback does not keep advising about a problem the agent already fixed.
pub fn count_unresolved_tool_outcome_failures(records: &[ToolCallRecord]) -> usize {
    unresolved_tool_outcome_failure_counts(records)
        .values()
        .copied()
        .sum()
}

/// Whether an executed operation carries affirmative typed success evidence.
///
/// `ToolCallRecord::ok` only proves that the tool transport/executor completed;
/// domain outcomes such as an empty search, a false predicate, or an
/// inconclusive pipeline are not positive validation receipts.  Callers that
/// authorize delivery must require this stronger fact instead of inferring
/// success from the absence of an unresolved failure.
pub fn tool_outcome_is_positive_success(record: &ToolCallRecord) -> bool {
    record.effective_disposition() == astra_services::session_journal::ToolCallDisposition::Executed
        && record.ok
        && effective_tool_result_class(record).as_deref() == Some("success")
        && record.exit_semantics.as_deref() == Some("success")
}

fn effective_tool_result_class(record: &ToolCallRecord) -> Option<String> {
    if let Some(class) = record
        .result_class
        .as_deref()
        .map(str::trim)
        .filter(|class| !class.is_empty())
    {
        return Some(class.to_string());
    }

    structured_tool_result_class(record).map(str::to_string)
}

fn structured_tool_result_class(record: &ToolCallRecord) -> Option<&'static str> {
    let value = parse_structured_tool_result(record)?;
    if record.name == "agent_fanout" || agent_fanout_result_looks_like(&value) {
        return agent_fanout_structured_result_class(&value);
    }
    if record.name == "agent" || agent_tool_result_looks_like(&value) {
        return agent_tool_structured_result_class(&value);
    }
    None
}

fn parse_structured_tool_result(record: &ToolCallRecord) -> Option<Value> {
    [
        record.result_full.as_deref(),
        record.result_preview.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(|raw| serde_json::from_str::<Value>(raw).ok())
}

fn apply_blocked_tool_failures(eval: &mut TurnEvaluation, records: &[ToolCallRecord]) {
    let blocked = unresolved_blocked_attempt_count(records);
    if blocked == 0 {
        return;
    }

    eval.signals
        .retain(|signal| !matches!(signal, EvalSignal::AllToolsHealthy));
    eval.signals
        .push(EvalSignal::BlockedToolCall { count: blocked });
    let penalty = (0.20 + 0.08 * blocked.saturating_sub(1) as f64).clamp(0.20, 0.45);
    eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
    eval.confidence = (eval.confidence + 0.15).clamp(0.0, 1.0);

    // A blocked invocation is an unresolved execution obligation. Successful
    // observations elsewhere can improve quality, but cannot make the
    // operational boundary itself successful. A later authoritative recovery
    // should replace/resolve the blocked record before terminal evaluation.
    eval.success = false;
}

fn blocked_resolution_key(record: &ToolCallRecord, record_index: usize) -> String {
    rejected_operation_key(record)
        .map(|key| key.to_string())
        .unwrap_or_else(|| format!("tool::{}::opaque::{record_index}", record.name))
}

/// Count rejected invocations that have not been resolved by a successful
/// execution of the same tool and arguments later in the ledger.
pub fn unresolved_blocked_attempt_count(records: &[ToolCallRecord]) -> usize {
    let mut unresolved = std::collections::BTreeMap::<String, usize>::new();
    for (record_index, record) in records.iter().enumerate() {
        let key = blocked_resolution_key(record, record_index);
        if record_is_rejected_attempt(record) {
            *unresolved.entry(key).or_insert(0) += 1;
        } else if record_was_executed(record) && record.ok {
            unresolved.remove(&key);
        }
    }
    unresolved.values().sum()
}

/// Count the consecutive rejected invocations at the end of the execution
/// ledger. A later executed call is evidence that the model continued through
/// a recovery path, even when the corrected call necessarily used different
/// arguments or a different tool. Terminal prose directly after a rejection
/// has no such boundary and needs one bounded recovery opportunity.
pub fn terminal_rejected_attempt_count(records: &[ToolCallRecord]) -> usize {
    records
        .iter()
        .rev()
        .take_while(|record| record_is_rejected_attempt(record))
        .count()
}

/// A rejected request is a user-visible failed attempt even though it never
/// reached an executor.  Keep it out of execution health/error-rate math, but
/// include it in the turn boundary so policy/admission failures cannot make a
/// turn look successful merely because the executor saw zero calls.
fn record_is_rejected_attempt(record: &ToolCallRecord) -> bool {
    !record.is_synthetic_placeholder()
        && (record.effective_disposition()
            == astra_services::session_journal::ToolCallDisposition::Rejected
            || record.was_blocked_by_policy())
}

fn apply_unresolved_tool_outcome_failures(eval: &mut TurnEvaluation, records: &[ToolCallRecord]) {
    let counts = unresolved_tool_outcome_failure_counts(records);
    let total: usize = counts.values().sum();
    if total == 0 {
        return;
    }

    eval.signals
        .retain(|signal| !matches!(signal, EvalSignal::AllToolsHealthy));
    for (class, count) in counts {
        eval.signals
            .push(EvalSignal::ToolOutcomeFailure { class, count });
    }
    let observed = records
        .iter()
        .filter(|record| record_was_executed(record) || record_is_rejected_attempt(record))
        .count();
    eval.signals.push(EvalSignal::ToolOutcomeFailureCoverage {
        unresolved: total,
        observed,
    });

    let penalty = (0.25 + 0.08 * total.saturating_sub(1) as f64).clamp(0.25, 0.50);
    eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
    eval.confidence = (eval.confidence + 0.15).clamp(0.0, 1.0);
    eval.success = false;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExplorationFamily {
    Diff,
    Search,
    Read,
}

impl ExplorationFamily {
    fn as_str(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Search => "search",
            Self::Read => "read",
        }
    }
}

fn classify_exploration_family(record: &ToolCallRecord) -> Option<ExplorationFamily> {
    let args = record.authoritative_args_full().unwrap_or("");
    match record.name.as_str() {
        name if is_diff_like_tool_call(name, args) => Some(ExplorationFamily::Diff),
        "read_file" => Some(ExplorationFamily::Read),
        "grep" | "rg" | "glob" => Some(ExplorationFamily::Search),
        "bash" if is_search_like_tool_call(&record.name, args) => Some(ExplorationFamily::Search),
        "bash" if extract_read_target(&record.name, args).is_some() => {
            Some(ExplorationFamily::Read)
        }
        _ => None,
    }
}

fn tool_action_is(args: &str, expected: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .is_some_and(|value| {
            value.get("action").and_then(serde_json::Value::as_str) == Some(expected)
        })
}

fn is_diff_like_tool_call(name: &str, args: &str) -> bool {
    if name == "git_diff" || (name == "git" && tool_action_is(args, "diff")) {
        return true;
    }
    if name != "bash" {
        return false;
    }

    use regex::Regex;
    use std::sync::OnceLock;

    static DIFF_CMD: OnceLock<Regex> = OnceLock::new();
    let diff_re =
        DIFF_CMD.get_or_init(|| Regex::new(r#"(^|[;&|]\s*)git\s+(--no-pager\s+)?diff\b"#).unwrap());
    diff_re.is_match(&bash_command_text(args))
}

fn longest_exploration_family_round_streak(
    records: &[ToolCallRecord],
) -> Option<(ExplorationFamily, usize)> {
    use std::collections::BTreeMap;

    #[derive(Clone, Copy, Default)]
    struct RoundState {
        family: Option<ExplorationFamily>,
        homogeneous: bool,
        count: usize,
    }

    let mut per_round: BTreeMap<u32, RoundState> = BTreeMap::new();
    for record in records {
        if !record_was_executed(record) {
            continue;
        }
        let Some(round) = record.round else { continue };
        let state = per_round.entry(round).or_insert(RoundState {
            family: None,
            homogeneous: true,
            count: 0,
        });
        state.count += 1;
        let Some(family) = classify_exploration_family(record) else {
            state.homogeneous = false;
            continue;
        };
        match state.family {
            None => state.family = Some(family),
            Some(existing) if existing == family && state.homogeneous => {}
            Some(_) => state.homogeneous = false,
        }
    }

    let mut best: Option<(ExplorationFamily, usize)> = None;
    let mut current_family: Option<ExplorationFamily> = None;
    let mut current_streak = 0usize;
    let mut prev_round: Option<u32> = None;

    for (&round, state) in &per_round {
        let round_family = if state.homogeneous && state.count >= 2 {
            state.family
        } else {
            None
        };
        let adjacent = prev_round.is_none_or(|prev| round == prev + 1);
        match round_family {
            Some(family) if adjacent && current_family == Some(family) => {
                current_streak += 1;
            }
            Some(family) => {
                current_family = Some(family);
                current_streak = 1;
            }
            None => {
                current_family = None;
                current_streak = 0;
            }
        }
        if let Some(family) = current_family {
            match best {
                Some((_, best_streak)) if best_streak >= current_streak => {}
                _ => best = Some((family, current_streak)),
            }
        }
        prev_round = Some(round);
    }

    best
}

/// Return the dominant exploratory-family streak across consecutive
/// multi-call rounds. Exposed so runtime mid-loop guards can reuse the same
/// family/streak definition as passive turn evaluation.
pub fn exploration_family_round_streak(
    records: &[ToolCallRecord],
) -> Option<(&'static str, usize)> {
    longest_exploration_family_round_streak(records)
        .map(|(family, streak)| (family.as_str(), streak))
}

fn bash_command_text(args: &str) -> String {
    let trimmed = args.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(command) = v.get("command").and_then(|c| c.as_str())
    {
        return command.trim().to_string();
    }
    trimmed.to_string()
}

fn is_search_like_tool_call(name: &str, args: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;

    if matches!(name, "grep" | "rg") {
        return true;
    }
    if name != "bash" {
        return false;
    }
    static SEARCH_CMD: OnceLock<Regex> = OnceLock::new();
    let search_re = SEARCH_CMD
        .get_or_init(|| Regex::new(r#"(^|[;&|]\s*|\s+)(git\s+grep|grep|rg|find)\b"#).unwrap());
    search_re.is_match(&bash_command_text(args))
}

/// Count grep/rg/find-like search calls in a turn. Intended for passive
/// post-mortem evaluation only — this signal is intentionally broad, so the
/// threshold is set conservatively and the penalty is mild.
pub fn count_search_fanout(records: &[ToolCallRecord]) -> usize {
    records
        .iter()
        .filter(|rec| record_was_executed(rec))
        .filter(|rec| {
            is_search_like_tool_call(&rec.name, rec.authoritative_args_full().unwrap_or(""))
        })
        .count()
}

const MAX_SHELL_LEX_BYTES: usize = 32 * 1024;
const MAX_NESTED_SHELL_SCAN_BYTES: usize = 16 * 1024;
const MAX_NESTED_SHELL_DEPTH: usize = 16;

/// Top-level shell control-flow operator retained for evidence consumers.
/// This is metadata only; it never authorizes or blocks command execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellControlOp {
    And,
    Or,
    Sequence,
    Neutral,
}

/// Split a shell command at validated top-level sequencing operators.  The
/// lexer is shared by evidence consumers so they do not each invent a partial
/// quote/substitution parser.  Unsupported or malformed shell syntax returns
/// `None`; callers must then keep the command executable but decline to infer
/// a completion receipt.
pub fn split_shell_control_segments(command: &str) -> Option<Vec<&str>> {
    split_shell_segments(command, true, false)
}

/// Split a command while retaining the operator which followed each segment.
/// The ordinary public splitter intentionally exposes only segment text for
/// callers that need a best-effort read/mutation classification.  Completion
/// receipts additionally need the control-flow fact: a validator followed by
/// `;` or `||` is not a proof that the overall command succeeded, whereas a
/// validator followed by `&&` remains on the successful path.  Comments are
/// neutral; a newline after a comment is represented by the following empty
/// segment and therefore still applies its sequence semantics.
pub fn split_shell_control_segments_with_ops(command: &str) -> Option<Vec<(&str, ShellControlOp)>> {
    let delimiters = top_level_shell_delimiters(command, true, false)?;
    if delimiters.is_empty() {
        return Some(vec![(command, ShellControlOp::Neutral)]);
    }
    let bytes = command.as_bytes();
    let mut segments = Vec::with_capacity(delimiters.len() + 1);
    let mut start = 0usize;
    for (index, width) in delimiters {
        let op = match bytes.get(index..index + width) {
            Some(b"&&") => ShellControlOp::And,
            Some(b"||") => ShellControlOp::Or,
            Some(b";") | Some(b"\n") | Some(b"\r\n") => ShellControlOp::Sequence,
            // A comment delimiter consumes its text up to (but not including)
            // a following newline. It does not itself change command status.
            _ => ShellControlOp::Neutral,
        };
        segments.push((&command[start..index], op));
        start = index + width;
    }
    segments.push((&command[start..], ShellControlOp::Neutral));
    Some(segments)
}

/// Parse one already-segmented shell command into literal words.  This is a
/// deliberately static lexer, not a shell interpreter: it preserves quoted
/// whitespace, rejects expansion/operators/escapes, and returns `None` for
/// anything whose destination cannot be known without executing a shell.
pub fn split_static_shell_words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut token_started = false;
    let chars = segment.chars().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                if active_quote == '"' && matches!(ch, '$' | '`' | '\\') {
                    return None;
                }
                current.push(ch);
            }
            token_started = true;
            index += 1;
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                token_started = true;
            }
            ch if ch.is_whitespace() => {
                if token_started {
                    words.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            '>' => {
                if token_started {
                    words.push(std::mem::take(&mut current));
                    token_started = false;
                }
                if chars.get(index + 1) == Some(&'>') {
                    words.push(">>".to_string());
                    index += 1;
                } else {
                    words.push(">".to_string());
                }
            }
            '$' | '`' | '~' | '*' | '?' | '[' | ']' | '\\' | ';' | '|' | '&' | '<' | '(' | ')'
            | '\n' | '\r' => return None,
            _ => {
                current.push(ch);
                token_started = true;
            }
        }
        index += 1;
    }
    if quote.is_some() {
        return None;
    }
    if token_started {
        words.push(current);
    }
    Some(words)
}

fn split_top_level_pipeline_segments(command: &str) -> Option<Vec<&str>> {
    split_shell_segments(command, false, true)
}

/// Split a shell command at top-level pipelines using the same quote,
/// substitution, and malformed-input rules as the evidence evaluator.
/// Consumers may use this for scope/receipt analysis, but it never authorizes
/// execution and returns `None` for syntax it cannot prove.
pub fn split_shell_pipeline_segments(command: &str) -> Option<Vec<&str>> {
    split_top_level_pipeline_segments(command)
}

fn split_shell_segments(
    command: &str,
    include_control: bool,
    include_pipeline: bool,
) -> Option<Vec<&str>> {
    let delimiters = top_level_shell_delimiters(command, include_control, include_pipeline)?;
    if delimiters.is_empty() {
        return Some(vec![command]);
    }
    let mut segments = Vec::with_capacity(delimiters.len() + 1);
    let mut start = 0;
    for (index, width) in delimiters {
        segments.push(&command[start..index]);
        start = index + width;
    }
    segments.push(&command[start..]);
    Some(segments)
}

/// Find the closing parenthesis for a `$()` beginning at `open_index` and
/// return `(exclusive_end, body_end)`. This is a lexical matcher only; it
/// rejects unsupported backticks, here-docs, malformed quotes, and excessive
/// nesting rather than guessing at shell semantics.
fn command_substitution_end(command: &str, open_index: usize) -> Option<(usize, usize)> {
    let bytes = command.as_bytes();
    if open_index + 1 >= bytes.len() || bytes[open_index] != b'$' || bytes[open_index + 1] != b'(' {
        return None;
    }
    let mut depth = 1usize;
    let mut quote = None;
    let mut escaped = false;
    let mut index = open_index + 2;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if byte == b'`' {
            return None;
        }
        if let Some(active_quote) = quote {
            if active_quote == b'\'' {
                if byte == active_quote {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if byte == b'"' {
                quote = None;
                index += 1;
                continue;
            }
            if byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
                depth += 1;
                if depth > MAX_NESTED_SHELL_DEPTH {
                    return None;
                }
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if byte == b'#' && shell_comment_starts(bytes, index) {
            index = shell_comment_end(bytes, index);
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte == b'`' || bytes.get(index..index + 2) == Some(b"<<") {
            return None;
        }
        if byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
            depth += 1;
            if depth > MAX_NESTED_SHELL_DEPTH {
                return None;
            }
            index += 2;
            continue;
        }
        match byte {
            b'(' => {
                depth += 1;
                if depth > MAX_NESTED_SHELL_DEPTH {
                    return None;
                }
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((index + 1, index));
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Lightweight shell lexer used only for evidence segmentation.  It tracks
/// quotes, escapes, and parenthesis depth so control operators inside `$()`
/// are never mistaken for outer sequencing.  It intentionally does not
/// execute or fully parse shell grammar; malformed/oversized input returns
/// `None` so callers fail closed rather than treating the whole input as one
/// apparently safe segment.
fn top_level_shell_delimiters(
    command: &str,
    include_control: bool,
    include_pipeline: bool,
) -> Option<Vec<(usize, usize)>> {
    if command.len() > MAX_SHELL_LEX_BYTES {
        return None;
    }
    let bytes = command.as_bytes();
    let mut delimiters = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if let Some(active_quote) = quote {
            if active_quote == b'\'' {
                if byte == active_quote {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
                index = command_substitution_end(command, index)?.0;
                continue;
            }
            if byte == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'#' && shell_comment_starts(bytes, index) {
            // Preserve the comment as a lexical separator so the caller
            // cannot let an unknown comment prefix erase a receipt. The
            // newline, if any, is processed on the next iteration.
            let end = shell_comment_end(bytes, index);
            delimiters.push((index, end - index));
            index = end;
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte == b'`' || bytes.get(index..index + 2) == Some(b"<<") {
            return None;
        }
        if byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
            index = command_substitution_end(command, index)?.0;
            continue;
        }
        if byte == b'(' || byte == b')' {
            // Shell grouping/arithmetic is intentionally outside this
            // evidence lexer. Unknown syntax must not be treated as a safe
            // separator; callers fail closed instead.
            return None;
        }
        if include_control && bytes.get(index..index + 2) == Some(b"&&") {
            delimiters.push((index, 2));
            index += 2;
            continue;
        }
        if include_control && bytes.get(index..index + 2) == Some(b"||") {
            delimiters.push((index, 2));
            index += 2;
            continue;
        }
        if include_control && byte == b';' {
            delimiters.push((index, 1));
            index += 1;
            continue;
        }
        if include_control && byte == b'\n' {
            delimiters.push((index, 1));
            index += 1;
            continue;
        }
        if include_control && byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            delimiters.push((index, 2));
            index += 2;
            continue;
        }
        if include_pipeline && byte == b'|' {
            delimiters.push((index, 1));
            index += 1;
            continue;
        }
        if byte == b'&' {
            // Preserve the two benign file-descriptor redirects used by
            // ordinary compiler/test commands. They are not sequencing or
            // background operators and are normalized by the caller.
            if index >= 2
                && bytes[index - 1] == b'>'
                && matches!(
                    (bytes[index - 2], bytes.get(index + 1).copied()),
                    (b'2', Some(b'1')) | (b'1', Some(b'2'))
                )
            {
                index += 2;
                continue;
            }
            // `&&` was consumed above; a standalone background operator is
            // not modeled and therefore cannot yield positive evidence.
            return None;
        }
        index += 1;
    }
    if quote.is_some() || escaped {
        return None;
    }
    Some(delimiters)
}

/// Return the canonical operation prefix for a known build/test validation
/// command.  This is intentionally a positive, narrow predicate: unlike the
/// permission classifier (which must fail closed for unknown shell syntax), it
/// only reports validation evidence when the command family is recognized.
///
/// The normalized prefix is shared by evaluation and completion settlement so
/// a successful `python3 -m pytest`/`cargo test` receipt cannot be mistaken for
/// an opaque, potentially mutating shell call.
pub fn normalize_validation_prefix(name: &str, args: &str) -> Option<String> {
    if name != "bash" {
        return None;
    }
    let command = bash_command_text(args);
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    bash_command_post_mutation_validation_prefix(command)
}

fn normalized_validation_segment(segment: &str) -> Option<String> {
    if validation_segment_has_meta_option(segment) {
        return None;
    }
    let lower = segment.to_ascii_lowercase();
    let recognized = lower.starts_with("cargo check ")
        || lower == "cargo check"
        || lower.starts_with("cargo test ")
        || lower == "cargo test"
        || lower.starts_with("cargo build ")
        || lower == "cargo build"
        || lower.starts_with("npx tsc --noemit")
        || lower == "tsc --noemit"
        || lower.starts_with("tsc --noemit ")
        || lower == "pytest"
        || lower.starts_with("pytest ")
        || lower == "python -m pytest"
        || lower.starts_with("python -m pytest ")
        || lower == "python3 -m pytest"
        || lower.starts_with("python3 -m pytest ")
        || lower == "python -m unittest"
        || lower.starts_with("python -m unittest ")
        || lower == "python3 -m unittest"
        || lower.starts_with("python3 -m unittest ")
        || lower == "python -m build"
        || lower.starts_with("python -m build ")
        || lower == "python3 -m build"
        || lower.starts_with("python3 -m build ")
        || lower.starts_with("python setup.py build ")
        || lower == "python setup.py build"
        || lower.starts_with("python setup.py build_ext ")
        || lower == "python setup.py build_ext"
        || lower.starts_with("python3 setup.py build ")
        || lower == "python3 setup.py build"
        || lower.starts_with("python3 setup.py build_ext ")
        || lower == "python3 setup.py build_ext"
        || lower == "npm test"
        || lower.starts_with("npm test ")
        || lower == "npm run build"
        || lower.starts_with("npm run build ")
        || lower == "go test"
        || lower.starts_with("go test ");
    recognized.then_some(segment.to_string())
}

/// A framework command's help/version mode is an informational query, not a
/// validation receipt. Keep this check provider- and task-neutral: it applies
/// to the common validation command families, while leaving ordinary reader
/// option semantics to the shell-observation classifier.
fn validation_segment_has_meta_option(segment: &str) -> bool {
    // Use the same static shell lexer as the path/redirect classifiers.  A
    // completion receipt must never be inferred from a command whose argv is
    // dynamic or malformed: `cargo test "$MODE"` could resolve to a metadata
    // query, and quoted spellings such as `cargo test '--help'` must retain
    // their option semantics.  This remains evidence-only; the command is
    // still executable under the normal permission policy.
    let Some(words) = split_static_shell_words(segment) else {
        // An opaque outer wrapper (for example an assignment containing a
        // command substitution) is handled by the nested-observation
        // classifier.  Only reject an unparseable command when its first
        // static token is itself a known validation family; otherwise this
        // check would turn every dynamic reader into a metadata failure.
        let first = segment
            .split_whitespace()
            .next()
            .map(|word| word.trim_matches(['\'', '"']))
            .map(str::to_ascii_lowercase);
        return first.is_some_and(|first| {
            matches!(
                first.as_str(),
                "cargo" | "go" | "npm" | "npx" | "pytest" | "python" | "python3" | "tsc"
            )
        });
    };
    let Some(first) = words.first().map(|word| word.to_ascii_lowercase()) else {
        return false;
    };
    if !matches!(
        first.as_str(),
        "cargo" | "go" | "npm" | "npx" | "pytest" | "python" | "python3" | "tsc"
    ) {
        return false;
    }
    words.iter().skip(1).any(|word| match word.as_str() {
        "-h" | "-?" | "--help" | "--version" | "--usage" => true,
        // Cargo/Go/Pytest use lower-case -v for verbose output, while the
        // TypeScript/npm frontends use it as a version query. Cargo's -V is
        // the version query; keep short-option meaning family-specific.
        "-v" => matches!(first.as_str(), "tsc" | "npx" | "npm"),
        "-V" => true,
        _ => false,
    })
}

/// Return the canonical build/test receipt that occurs after the final
/// possible mutation barrier in a shell command.  A validator before a later
/// writer is deliberately not returned; callers must not combine an old
/// project-wide prefix with a new, weaker observation.
pub fn bash_command_post_mutation_validation_prefix(command: &str) -> Option<String> {
    let mut latest = None;
    let control_segments = split_shell_control_segments_with_ops(command)?;
    for (segment_index, (raw_segment, op_after)) in control_segments.iter().enumerate() {
        let pipeline_segments = split_top_level_pipeline_segments(raw_segment)?;
        let mut pipeline_latest = None;
        let mut pipeline_blocked = false;
        for pipeline_segment in pipeline_segments {
            let segment = pipeline_segment.trim();
            if segment.is_empty() {
                continue;
            }
            let segment = segment.strip_suffix("2>&1").unwrap_or(segment).trim();
            let segment = segment.strip_suffix("1>&2").unwrap_or(segment).trim();
            let normalized = segment.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                continue;
            }
            // `cd`/`set` are neutral only when they are pure control
            // statements. A redirect on a control segment is still a write
            // and, in a pipeline, runs concurrently with the validator.
            if shell_segment_has_non_benign_redirect(segment) {
                pipeline_blocked = true;
                break;
            }
            if is_shell_control_segment(&normalized) {
                continue;
            }
            if let Some(prefix) = normalized_validation_segment(&normalized) {
                pipeline_latest = Some(prefix);
                continue;
            }
            if is_positive_validation_segment(&normalized)
                || output_wrapper_without_substitution(&normalized)
                || nested_shell_validation_is_positive(&normalized)
                || is_status_neutral_segment(&normalized)
                || crate::cloud_approval_policy::bash_command_is_read_only(&normalized)
            {
                continue;
            }
            // Pipeline stages execute concurrently. An unknown or mutating
            // stage cannot be ordered before a validator merely because the
            // validator appears later in the text; treat the whole pipeline
            // as a barrier instead of manufacturing a post-mutation receipt.
            pipeline_blocked = true;
            break;
        }
        if pipeline_blocked {
            latest = None;
        } else if let Some(prefix) = pipeline_latest {
            latest = Some(prefix);
        }
        // A successful `&&` continuation is conditional on the validator
        // succeeding, so it does not mask that receipt. `;` and `||` both
        // permit a later command to run after a failed validator and must
        // close the evidence epoch. Empty segments are significant here: a
        // newline following a comment appears as an empty segment.
        let sequence_has_rhs = control_segments[segment_index + 1..]
            .iter()
            .any(|(segment, _)| {
                let segment = segment.trim();
                !segment.is_empty() && !segment.starts_with('#')
            });
        if matches!(op_after, ShellControlOp::Or)
            || (matches!(op_after, ShellControlOp::Sequence) && sequence_has_rhs)
        {
            latest = None;
        }
    }
    latest
}

/// Return whether a compound shell command contains a positive validation
/// receipt after its last potentially state-changing segment.
///
/// Permission classification intentionally treats unknown shell syntax as
/// unsafe.  Completion evidence has a different question: a command such as
/// `prepare; deploy; cat result; curl endpoint` can still provide a useful
/// post-change observation, while `cat result; deploy` cannot.  Keep this
/// small and provider-neutral: unknown segments are barriers, known
/// validation/read commands are receipts, and shell control wrappers are
/// neutral.  This is evidence classification only; it never grants
/// execution authority or changes the shell safety policy.
pub fn bash_command_has_post_mutation_validation(command: &str) -> bool {
    let mut validation_after_barrier = false;
    let mut strict_receipt = false;
    let Some(control_segments) = split_shell_control_segments_with_ops(command) else {
        return false;
    };
    for (segment_index, (raw_segment, op_after)) in control_segments.iter().enumerate() {
        let Some(pipeline_segments) = split_top_level_pipeline_segments(raw_segment) else {
            return false;
        };
        let mut pipeline_validation = None;
        let mut pipeline_blocked = false;
        for pipeline_segment in pipeline_segments {
            let segment = pipeline_segment.trim();
            if segment.is_empty() {
                continue;
            }
            let segment = segment.strip_suffix("2>&1").unwrap_or(segment).trim();
            let segment = segment.strip_suffix("1>&2").unwrap_or(segment).trim();
            let normalized = segment.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                continue;
            }
            // Keep control-wrapper handling behind the raw redirect barrier;
            // otherwise `cd ... > out | cargo test` or `set -e > out | ...`
            // could manufacture a validation receipt.
            if shell_segment_has_non_benign_redirect(segment) {
                pipeline_blocked = true;
                break;
            }
            if is_shell_control_segment(&normalized) {
                continue;
            }
            if normalized_validation_segment(&normalized).is_some() {
                pipeline_validation = Some(true);
                continue;
            }
            if is_positive_validation_segment(&normalized) {
                pipeline_validation = Some(is_strict_local_validation_segment(&normalized));
                continue;
            }

            if is_status_neutral_segment(&normalized) {
                continue;
            }

            // Plain output of an already-computed shell variable is a neutral
            // presentation step. It must not erase a receipt observed
            // immediately before it, while command substitutions still go
            // through the stricter nested recognizer below.
            if output_wrapper_without_substitution(&normalized) {
                continue;
            }

            // A verification command is often wrapped in a harmless shell
            // presentation expression, for example `echo "$(curl …)"` or
            // `value=$(test -f …)`. The permission classifier must still
            // reject command substitution as unsafe syntax, but completion
            // evidence can inspect the nested command without granting it
            // execution authority. Only allow known read-only
            // wrappers/assignments; a redirect or an unknown outer command
            // remains a mutation barrier.
            if nested_shell_validation_is_positive(&normalized) {
                pipeline_validation = Some(false);
                continue;
            }

            // A command not proven read-only is a possible mutation barrier.
            // It includes unknown Python/Perl helpers and writes to external
            // systems such as `git push`; a later receipt is still useful, but
            // an earlier receipt must not survive it.
            if !crate::cloud_approval_policy::bash_command_is_read_only(&normalized) {
                pipeline_blocked = true;
                break;
            }
        }
        if pipeline_blocked {
            // Pipeline stages execute concurrently. A later validator cannot
            // prove that it ran after an earlier writer in the same pipeline.
            validation_after_barrier = false;
            strict_receipt = false;
        } else if let Some(strict) = pipeline_validation {
            validation_after_barrier = true;
            strict_receipt = strict;
        }
        let sequence_has_rhs = control_segments[segment_index + 1..]
            .iter()
            .any(|(segment, _)| {
                let segment = segment.trim();
                !segment.is_empty() && !segment.starts_with('#')
            });
        if strict_receipt
            && (matches!(op_after, ShellControlOp::Or)
                || (matches!(op_after, ShellControlOp::Sequence) && sequence_has_rhs))
        {
            validation_after_barrier = false;
            strict_receipt = false;
        }
    }
    validation_after_barrier
}

fn is_status_neutral_segment(segment: &str) -> bool {
    let normalized = segment.trim().to_ascii_lowercase();
    normalized == "true" || normalized == ":"
}

fn is_strict_local_validation_segment(segment: &str) -> bool {
    let Some(first) = segment.split_whitespace().next() else {
        return false;
    };
    matches!(first.to_ascii_lowercase().as_str(), "cmp" | "diff")
        || (matches!(
            first.to_ascii_lowercase().as_str(),
            "sha256sum" | "sha512sum"
        ) && segment.split_whitespace().any(|word| word == "-c"))
}

/// Return whether a shell segment contains a positive validation receipt in
/// command substitution(s) under a presentation-only wrapper.  This is a
/// deliberately small lexical recognizer, not a shell interpreter: it tracks
/// balanced `$()` and quotes, delegates each body back to the same
/// provider-neutral evidence predicate, and refuses writes/unknown wrappers.
fn nested_shell_validation_is_positive(segment: &str) -> bool {
    if segment.contains('>') {
        return false;
    }
    let Some(bodies) = command_substitution_bodies(segment) else {
        return false;
    };
    if bodies.is_empty() || !observation_wrapper_prefix(segment) {
        return false;
    }

    let mut saw_receipt = false;
    for body in bodies {
        if bash_command_has_post_mutation_validation(body) {
            saw_receipt = true;
        } else if !crate::cloud_approval_policy::bash_command_is_read_only(body) {
            // A nested command that is neither a known receipt nor a known
            // read-only formatter is a possible post-receipt mutation.
            return false;
        }
    }
    saw_receipt
}

fn output_wrapper_without_substitution(segment: &str) -> bool {
    if segment.contains("$(") || segment.contains('`') || segment.contains('>') {
        return false;
    }
    matches!(
        segment.split_whitespace().next().unwrap_or_default(),
        "echo" | "printf"
    )
}

/// Presentation-only shell forms for which nested command output is itself
/// an observation.  Keep this structural (assignments and standard output),
/// rather than matching task words, paths, or frameworks.
fn observation_wrapper_prefix(segment: &str) -> bool {
    let first = segment.split_whitespace().next().unwrap_or_default();
    if matches!(first, "echo" | "printf" | "test" | "[") {
        return true;
    }
    // A plain shell assignment (`name=$(...)`) does not execute an outer
    // command and is safe to use as an observation carrier.  Reject `$` in
    // the variable name so arbitrary command prefixes cannot masquerade as
    // assignments.
    let Some((name, _)) = first.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            (index == 0 && (byte == b'_' || byte.is_ascii_alphabetic()))
                || (index > 0 && (byte == b'_' || byte.is_ascii_alphanumeric()))
        })
}

/// Extract top-level command-substitution bodies while respecting quotes,
/// escapes, and nested parentheses. `None` means an unterminated `$()` was
/// found; an empty vector means the segment has no command substitutions.
fn command_substitution_bodies(command: &str) -> Option<Vec<&str>> {
    if command.len() > MAX_NESTED_SHELL_SCAN_BYTES
        || command.contains('`')
        || command.contains("<<")
    {
        return None;
    }
    let bytes = command.as_bytes();
    let mut bodies = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if byte == b'#' && shell_comment_starts(bytes, index) {
            index = shell_comment_end(bytes, index);
            continue;
        }
        if let Some(active_quote) = quote {
            // `$()` is evaluated inside double quotes, but not inside single
            // quotes. Let the normal substitution branch below handle it.
            if active_quote == b'\'' || (byte != b'$' || bytes.get(index + 1) != Some(&b'(')) {
                if byte == active_quote {
                    quote = None;
                }
                index += 1;
                continue;
            }
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte != b'$' || bytes.get(index + 1) != Some(&b'(') {
            index += 1;
            continue;
        }

        let start = index + 2;
        let (end, body_end) = command_substitution_end(command, index)?;
        bodies.push(&command[start..body_end]);
        if bodies.len() > MAX_NESTED_SHELL_DEPTH {
            return None;
        }
        index = end;
    }
    quote.is_none().then_some(bodies)
}

fn shell_comment_starts(bytes: &[u8], index: usize) -> bool {
    if bytes.get(index) != Some(&b'#') {
        return false;
    }
    index == 0
        || matches!(
            bytes[index - 1],
            b' ' | b'\t' | b'\n' | b'\r' | b';' | b'|' | b'&' | b'('
        )
}

fn shell_comment_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
        index += 1;
    }
    index
}

fn is_shell_control_segment(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    lower == "set -e"
        || lower == "set -u"
        || lower == "set -o pipefail"
        || lower.starts_with("set -e ")
        || lower.starts_with("set -u ")
        || lower.starts_with("set -o pipefail ")
        || lower.starts_with("cd ")
        || lower == "cd"
}

fn is_positive_validation_segment(segment: &str) -> bool {
    if validation_segment_has_meta_option(segment) || shell_segment_has_non_benign_redirect(segment)
    {
        return false;
    }
    let lower = segment.to_ascii_lowercase();
    let mut words = lower.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    match first {
        "cat" | "head" | "tail" | "grep" | "rg" | "find" | "ls" | "stat" | "file" | "sha256sum"
        | "sha512sum" | "cmp" | "diff" | "test" | "[" | "curl" | "wget" => {
            // Network clients are receipts only for read requests.  Do not
            // treat a POST/upload form as observation of the workspace.
            if matches!(first, "curl" | "wget")
                && lower.split_whitespace().any(|word| {
                    matches!(
                        word,
                        "-d" | "--data"
                            | "--data-raw"
                            | "--data-binary"
                            | "--post-data"
                            | "--upload-file"
                            | "-t"
                            | "--method=post"
                            | "--method=put"
                            | "--method=patch"
                            | "--method=delete"
                    ) || (word == "-x" || word == "--request")
                })
            {
                return false;
            }
            true
        }
        "git" => matches!(
            words.next(),
            Some("status" | "diff" | "show" | "log" | "ls-files" | "branch" | "remote")
        ),
        "openssl" => matches!(words.next(), Some("x509" | "verify" | "s_client")),
        "nginx" => words.next() == Some("-t"),
        "systemctl" => matches!(words.next(), Some("status" | "is-active" | "is-enabled")),
        "cargo" => matches!(words.next(), Some("check" | "test" | "build")),
        "pytest" => true,
        "python" | "python3" => {
            let Some(second) = words.next() else {
                return false;
            };
            match second {
                "-m" => matches!(words.next(), Some("pytest" | "unittest" | "build")),
                // Building a local extension is also a validation receipt:
                // the command must successfully compile the changed source
                // before the completion ledger can settle it.
                "setup.py" => {
                    matches!(words.next(), Some("build" | "build_ext" | "check" | "test"))
                }
                _ => false,
            }
        }
        "npm" => {
            matches!(words.next(), Some("test"))
                || (matches!(words.next(), Some("run"))
                    && matches!(words.next(), Some("test" | "build")))
        }
        "go" => words.next() == Some("test"),
        _ => false,
    }
}

/// Output redirection changes state even when the command name is a reader.
/// The two benign descriptor-forwarding forms are stripped by callers before
/// this predicate is used; an unparsed stage is conservatively treated as a
/// possible redirect/mutation rather than a receipt.
pub fn shell_segment_has_non_benign_redirect(segment: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for byte in segment.trim().bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'>' => return true,
            _ => {}
        }
    }
    false
}

/// Largest unresolved retry count for one normalized validation operation.
///
/// Output-shaping suffixes are ignored and an authoritative workspace
/// mutation resets the operation, so runtime feedback can reuse the terminal
/// evaluator's definition without treating a changed program as a retry. A
/// retry count of 2 means the same prefix ran 3 times in one no-mutation
/// window.
pub fn max_redundant_validation_retries(records: &[ToolCallRecord]) -> usize {
    use std::collections::HashMap;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut best = 0usize;
    for rec in records {
        if !record_was_executed(rec) {
            continue;
        }
        let args = rec.authoritative_args_full().unwrap_or("");
        if is_mutation_for_redundant_read(&rec.name, args) {
            seen.clear();
            continue;
        }
        if let Some(prefix) = normalize_validation_prefix(&rec.name, args) {
            let entry = seen.entry(prefix).or_insert(0);
            *entry += 1;
            best = best.max(entry.saturating_sub(1));
        }
    }
    best
}

/// Parsed read target for redundancy detection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadRange {
    file: String,
    /// Line range, or `None` for whole-file reads.
    range: Option<(u32, u32)>,
}

#[derive(Debug, Default)]
struct ReadCoverage {
    unbounded_requested: bool,
    /// Sorted, disjoint inclusive ranges requested from the unchanged file.
    ranges: Vec<(u32, u32)>,
}

impl ReadCoverage {
    fn already_covers(&self, current: &ReadRange) -> bool {
        // An unbounded request is not proof that the whole file reached the
        // model: read_file may truncate or return an outline at its output
        // boundary. A repeated unbounded request asks for the same projection
        // again, but it must not make later targeted ranges look known.
        let Some((current_start, current_end)) = current.range else {
            return self.unbounded_requested;
        };
        self.ranges
            .iter()
            .any(|(start, end)| *start <= current_start && *end >= current_end)
    }

    fn record(&mut self, current: &ReadRange) {
        let Some((mut start, mut end)) = current.range else {
            self.unbounded_requested = true;
            return;
        };

        let mut merged = Vec::with_capacity(self.ranges.len().saturating_add(1));
        let mut inserted = false;
        for (prior_start, prior_end) in self.ranges.drain(..) {
            if prior_end.saturating_add(1) < start {
                merged.push((prior_start, prior_end));
            } else if end.saturating_add(1) < prior_start {
                if !inserted {
                    merged.push((start, end));
                    inserted = true;
                }
                merged.push((prior_start, prior_end));
            } else {
                start = start.min(prior_start);
                end = end.max(prior_end);
            }
        }
        if !inserted {
            merged.push((start, end));
        }
        self.ranges = merged;
    }
}

/// Best-effort extraction of a read target from a tool-call's full args.
/// Returns `None` for tools that don't read file content (grep/ls/glob),
/// for ambiguous bash commands, and for parse failures. Recognized:
///   - `bash` with `sed -n '<a>,<b>p' <file>`
///   - `bash` with bare `cat <file>` (no shell redirection / pipe input)
///   - `read_file` tool with JSON args like `{"path":"<f>","start_line":a,"end_line":b}`
fn extract_read_target(name: &str, args: &str) -> Option<ReadRange> {
    use regex::Regex;
    use std::sync::OnceLock;
    static SED_RANGE: OnceLock<Regex> = OnceLock::new();
    static CAT_FILE: OnceLock<Regex> = OnceLock::new();

    if name == "read_file" {
        // Prefer JSON parsing — `read_file` args_full is always JSON.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args.trim()) {
            let path = v.get("path").and_then(|p| p.as_str())?.to_string();
            let s = v
                .get("start_line")
                .and_then(|n| n.as_u64())
                .map(|n| n as u32);
            let e = v.get("end_line").and_then(|n| n.as_u64()).map(|n| n as u32);
            let range = s.zip(e);
            return Some(ReadRange { file: path, range });
        }
        return None;
    }
    if name != "bash" {
        return None;
    }
    // `sed -n 'A,Bp' file`  (with or without quotes around the range)
    let sed_re = SED_RANGE
        .get_or_init(|| Regex::new(r#"\bsed\s+-n\s+['"]?(\d+),(\d+)p['"]?\s+(\S+)"#).unwrap());
    if let Some(c) = sed_re.captures(args) {
        let s: u32 = c.get(1)?.as_str().parse().ok()?;
        let e: u32 = c.get(2)?.as_str().parse().ok()?;
        let f = c
            .get(3)?
            .as_str()
            .trim_matches(|x| x == '\'' || x == '"')
            .to_string();
        return Some(ReadRange {
            file: f,
            range: Some((s, e)),
        });
    }
    // Bare `cat <file>` — must NOT be part of a pipeline that accepts
    // input (heredoc, `<`, `<<`) and must NOT be a mutation (`>`/`>>`/`tee`).
    if args.contains('>') || args.contains("<<") {
        return None;
    }
    let cat_re = CAT_FILE.get_or_init(|| Regex::new(r#"\bcat\s+(\S+)"#).unwrap());
    if let Some(c) = cat_re.captures(args)
        && let Some(f) = c.get(1)
    {
        let f = f
            .as_str()
            .trim_matches(|x| x == '\'' || x == '"')
            .to_string();
        // Skip if the "file" looks like an option (e.g. `-n`).
        if !f.starts_with('-') {
            return Some(ReadRange {
                file: f,
                range: None,
            });
        }
    }
    None
}

/// Whether a tool call mutates workspace state in a way that justifies
/// a re-read of the same content. Conservative — any plausible mutation
/// clears the per-file read history so we don't over-flag legitimate
/// "edit then verify" patterns.
fn is_mutation_for_redundant_read(name: &str, args: &str) -> bool {
    matches!(
        name,
        "str_replace" | "multi_edit" | "write_file" | "create_file" | "delete_file" | "apply_patch"
    ) || (name == "bash" && bash_args_look_mutating(args))
}

fn bash_args_look_mutating(args: &str) -> bool {
    // Output redirection or in-place mutation. Ignore fd redirects like
    // `2>&1` / `1>&2` — they shape output but do not mutate workspace.
    let bytes = args.as_bytes();
    let has_write_redirect = bytes.iter().enumerate().any(|(i, b)| {
        *b == b'>'
            && bytes
                .get(i + 1)
                .is_some_and(|next| *next != b'&' && *next != b'>')
    });
    if args.contains(">>")
        || args.contains("|& tee")
        || args.contains("| tee")
        || has_write_redirect
    {
        return true;
    }
    // Common mutating commands as substrings (tolerates prefixes like
    // `cd /tmp && mv …` or `sudo rm …`).
    const MUT_NEEDLES: &[&str] = &[
        " mv ",
        " cp ",
        " rm ",
        "sed -i",
        "rmdir ",
        "mkdir ",
        "touch ",
        "chmod ",
        "chown ",
        "git commit",
        "git add",
        "git checkout",
        "git reset",
        "git rebase",
        "git merge",
        "git apply",
    ];
    let padded = format!(" {args} ");
    MUT_NEEDLES.iter().any(|n| padded.contains(n))
}

/// Best-effort extraction of the file targeted by a mutation, used to scope
/// per-file history clears. Returns `None` if the target file is unclear,
/// in which case the caller clears ALL per-file histories (conservative).
fn mutation_target_file(name: &str, args: &str) -> Option<String> {
    if matches!(
        name,
        "str_replace" | "multi_edit" | "write_file" | "create_file" | "delete_file" | "apply_patch"
    ) {
        // Astra's file mutation tools take JSON args with a `path` field.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args.trim()) {
            return v.get("path").and_then(|p| p.as_str()).map(String::from);
        }
    }
    None
}

/// Count redundant overlapping reads — see `EvalSignal::RedundantOverlappingReads`.
///
/// Public so the runtime can call it mid-loop for an advisory observation,
/// not just post-mortem. The runtime uses a slightly higher threshold than
/// the eval-signal threshold to err on the side of underkill for the
/// behavioral intervention.
pub fn count_redundant_overlapping_reads(records: &[ToolCallRecord]) -> usize {
    use std::collections::HashMap;
    let mut per_file: HashMap<String, ReadCoverage> = HashMap::new();
    let mut redundant = 0usize;
    for rec in records {
        if !record_was_executed(rec) {
            continue;
        }
        let args = rec.authoritative_args_full().unwrap_or("");
        if is_mutation_for_redundant_read(&rec.name, args) {
            // Mutation: invalidate the relevant file's read history (or all
            // file histories if we can't pinpoint the target).
            match mutation_target_file(&rec.name, args) {
                Some(f) => {
                    per_file.remove(&f);
                }
                None => per_file.clear(),
            }
            continue;
        }
        if let Some(target) = extract_read_target(&rec.name, args) {
            let coverage = per_file.entry(target.file.clone()).or_default();
            if coverage.already_covers(&target) {
                redundant += 1;
            }
            coverage.record(&target);
        }
    }
    redundant
}

/// Count redundant reads that are still actionable at the current boundary.
///
/// Unlike [`count_redundant_overlapping_reads`], this projection forgets a
/// file's prior redundancy after an authoritative workspace mutation.  The
/// historical counter remains appropriate for terminal audit; runtime policy
/// must not keep advising against behavior that the agent has already left.
pub fn count_active_redundant_overlapping_reads(records: &[ToolCallRecord]) -> usize {
    use std::collections::HashMap;

    let mut per_file: HashMap<String, ReadCoverage> = HashMap::new();
    let mut redundant_per_file: HashMap<String, usize> = HashMap::new();
    for rec in records {
        if !record_was_executed(rec) || !rec.ok {
            continue;
        }
        let args = rec.authoritative_args_full().unwrap_or("");
        if is_mutation_for_redundant_read(&rec.name, args) {
            match mutation_target_file(&rec.name, args) {
                Some(file) => {
                    per_file.remove(&file);
                    redundant_per_file.remove(&file);
                }
                None => {
                    per_file.clear();
                    redundant_per_file.clear();
                }
            }
            continue;
        }
        if let Some(target) = extract_read_target(&rec.name, args) {
            let coverage = per_file.entry(target.file.clone()).or_default();
            if coverage.already_covers(&target) {
                let count = redundant_per_file.entry(target.file.clone()).or_default();
                *count = count.saturating_add(1);
            }
            coverage.record(&target);
        }
    }
    redundant_per_file.values().copied().sum()
}

pub fn eval_signal_to_json(signal: &EvalSignal) -> serde_json::Value {
    eval_signal_to_json_with_thresholds(signal, EvaluationThresholds::default())
}

pub fn eval_signal_to_json_with_thresholds(
    signal: &EvalSignal,
    thresholds: EvaluationThresholds,
) -> serde_json::Value {
    match signal {
        EvalSignal::ToolErrorRate(rate) => json!({
            "kind": "tool_error_rate",
            "weight": rate,
        }),
        EvalSignal::EmptyToolOutput => json!({
            "kind": "empty_tool_output",
            "message": "At least one successful tool call returned minimal output",
        }),
        EvalSignal::StallDetected => json!({
            "kind": "stall_detected",
            "message": "TurnGuard recorded a stall or divergence event",
        }),
        EvalSignal::HighBudgetPressure => json!({
            "kind": "high_budget_pressure",
            "message": "Budget pressure crossed the high-pressure threshold",
        }),
        EvalSignal::RepeatToolCall(name) => json!({
            "kind": "repeat_tool_call",
            "tool": name,
            "message": format!("Repeated tool call pattern detected for `{name}`"),
        }),
        EvalSignal::VerdictWarning => json!({
            "kind": "verdict_warning",
            "message": "TurnGuard emitted a warning-or-higher verdict",
        }),
        EvalSignal::NoToolsNeeded => json!({
            "kind": "missing_needed_tools",
            "message": "The turn looked like a live query but completed without tool calls",
        }),
        EvalSignal::AllToolsHealthy => json!({
            "kind": "all_tools_healthy",
            "message": "All tool calls completed successfully with non-empty output",
        }),
        EvalSignal::NoOpToolResults(count) => json!({
            "kind": "noop_tool_results",
            "count": count,
            "message": format!(
                "Detected {count} tool result(s) that reused cached or already-known observations instead of fresh evidence"
            ),
        }),

        EvalSignal::RedundantOverlappingReads(count) => json!({
            "kind": "redundant_overlapping_reads",
            "count": count,
            "threshold": thresholds.redundant_overlapping_reads,
            "message": format!(
                "Detected {count} redundant read(s) with no intervening workspace mutation — the model likely re-read content already in context"
            ),
        }),
        EvalSignal::SearchFanout(count) => json!({
            "kind": "search_fanout",
            "count": count,
            "threshold": thresholds.search_fanout,
            "message": format!(
                "Detected {count} grep/rg/find-like search calls in one turn — the model likely fanned out exploratory search instead of narrowing or switching to direct reads"
            ),
        }),
        EvalSignal::RedundantValidationRetries(count) => json!({
            "kind": "redundant_validation_retries",
            "count": count,
            "threshold": thresholds.redundant_validation_retries,
            "message": format!(
                "Detected {count} redundant retries of the same heavy validation command with no intervening workspace mutation"
            ),
        }),
        EvalSignal::LlmRoundChurn {
            rounds,
            prompt_tokens,
        } => json!({
            "kind": "llm_round_churn",
            "rounds": rounds,
            "prompt_tokens": prompt_tokens,
            "threshold": thresholds.llm_round_churn,
            "message": format!(
                "Detected {rounds} LLM rounds in one user turn with {prompt_tokens} prompt tokens — this turn likely churned through repeated replanning/tool loops"
            ),
        }),
        EvalSignal::PromptGrowthChurn {
            first_prompt_tokens,
            max_prompt_tokens,
            delta_tokens,
        } => json!({
            "kind": "prompt_growth_churn",
            "first_prompt_tokens": first_prompt_tokens,
            "max_prompt_tokens": max_prompt_tokens,
            "delta_tokens": delta_tokens,
            "min_rounds": PROMPT_GROWTH_CHURN_MIN_ROUNDS,
            "min_delta_tokens": PROMPT_GROWTH_CHURN_MIN_DELTA_TOKENS,
            "message": format!(
                "Prompt tokens grew from {first_prompt_tokens} to {max_prompt_tokens} in one turn (Δ {delta_tokens}) — this looks like context ballooning across repeated rounds"
            ),
        }),
        EvalSignal::ExplorationFamilyChurn { family, streak } => json!({
            "kind": "exploration_family_churn",
            "family": family,
            "streak": streak,
            "threshold": thresholds.exploration_family_churn,
            "message": format!(
                "Detected {streak} consecutive {family}-dominant exploratory rounds — marginal evidence yield likely collapsed before the loop shifted toward synthesis or action"
            ),
        }),
        EvalSignal::HighCostLowYield {
            tool_calls,
            llm_rounds,
        } => json!({
            "kind": "high_cost_low_yield",
            "tool_calls": tool_calls,
            "llm_rounds": llm_rounds,
            "message": match llm_rounds {
                Some(rounds) => format!(
                    "The turn used {tool_calls} tool calls across {rounds} LLM rounds while still showing low-yield exploration signals — success/confidence were downgraded to avoid optimistic scoring"
                ),
                None => format!(
                    "The turn used {tool_calls} tool calls while still showing low-yield exploration signals — success/confidence were downgraded to avoid optimistic scoring"
                ),
            },
        }),
        EvalSignal::ToolOutcomeFailure { class, count } => json!({
            "kind": "tool_outcome_failure",
            "class": class,
            "count": count,
            "message": format!(
                "Detected {count} unresolved tool outcome failure(s) classified as `{class}`"
            ),
        }),
        EvalSignal::ToolOutcomeFailureCoverage {
            unresolved,
            observed,
        } => json!({
            "kind": "tool_outcome_failure_coverage",
            "unresolved": unresolved,
            "observed": observed,
            "message": format!(
                "{unresolved} unresolved classified tool outcome failure(s) across {observed} materially attempted call(s)"
            ),
        }),
        EvalSignal::BlockedToolCall { count } => json!({
            "kind": "blocked_tool_call",
            "count": count,
            "message": format!("Detected {count} blocked tool call(s)"),
        }),
    }
}
pub fn eval_signals_to_json_with_thresholds(
    signals: &[EvalSignal],
    thresholds: EvaluationThresholds,
) -> Vec<serde_json::Value> {
    signals
        .iter()
        .map(|signal| eval_signal_to_json_with_thresholds(signal, thresholds))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn build_turn_evaluation_journal_event(
    session_id: Option<&str>,
    turn: Option<u32>,
    source: &str,
    _input: &str,
    _recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    eval: &TurnEvaluation,
) -> JournalEvent {
    let tool_attempt_count = tool_call_records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder())
        .count();
    let tool_execution_count = tool_call_records
        .iter()
        .filter(|record| record_was_executed(record))
        .count();
    let tool_rejected_count = tool_call_records
        .iter()
        .filter(|record| {
            !record.is_synthetic_placeholder()
                && (matches!(
                    record.disposition,
                    Some(astra_services::session_journal::ToolCallDisposition::Rejected)
                ) || record.was_blocked_by_policy())
        })
        .count();
    let live_query = records_include_live_query(tool_call_records);
    let mut event = JournalEvent::turn_evaluation(
        session_id,
        turn,
        source,
        live_query,
        eval.success,
        eval.quality,
        eval.confidence,
        budget_pressure,
        stall_count,
        verdict_warning,
        tool_attempt_count,
        eval_signals_to_json_with_thresholds(&eval.signals, eval.thresholds),
    );
    if let Some(metadata) = event.metadata.as_mut().and_then(Value::as_object_mut) {
        metadata.insert(
            "tool_execution_count".to_string(),
            Value::from(tool_execution_count),
        );
        metadata.insert(
            "tool_rejected_count".to_string(),
            Value::from(tool_rejected_count),
        );
    }
    event
}

fn records_include_live_query(records: &[ToolCallRecord]) -> bool {
    records.iter().any(|record| {
        !record.is_synthetic_placeholder()
            && matches!(record.name.as_str(), "web_search" | "web_fetch")
    })
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::JournalEventType;

    fn ok_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            output_bytes: Some(500),
            no_op: false,
        }
    }

    fn err_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: false,
            ms: 50,
            error: Some("tool error".to_string()),
            output_bytes: None,
            no_op: false,
        }
    }

    fn empty_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            output_bytes: Some(0),
            no_op: false,
        }
    }

    fn noop_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: true,
            ms: 10,
            error: None,
            output_bytes: Some(120),
            no_op: true,
        }
    }

    #[test]
    fn blocked_tool_calls_fail_the_boundary_without_poisoning_execution_health() {
        let records = vec![ToolCallRecord {
            name: "memory".to_string(),
            ok: false,
            ms: 4,
            error: Some("blocked_tool: executor is unavailable: service_executor_required".into()),
            result_class: Some(
                astra_services::session_journal::BLOCKED_TOOL_RESULT_CLASS.to_string(),
            ),
            ..Default::default()
        }];

        let eval = evaluate_tool_call_records(
            "what were my previous inputs?",
            &[],
            &records,
            0,
            false,
            0.0,
        );

        assert!(!eval.success);
        assert!(
            !eval.signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::ToolErrorRate(rate) if (*rate - 1.0).abs() < f64::EPSILON)),
            "blocked calls did not execute and must not contribute to tool_error_rate: {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::BlockedToolCall { count: 1 })),
            "blocked calls need an explicit diagnostic signal: {:?}",
            eval.signals
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::AllToolsHealthy)),
            "blocked calls must not coexist with all_tools_healthy"
        );

        let event = build_turn_evaluation_journal_event(
            Some("session-blocked"),
            Some(1),
            "cli_repl",
            "what were my previous inputs?",
            &[],
            &records,
            0,
            false,
            0.0,
            &eval,
        );
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["tool_execution_count"], 0);
        assert_eq!(metadata["tool_rejected_count"], 1);
        assert!(
            metadata["signals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|signal| signal["kind"] == "blocked_tool_call"),
            "journal metadata must surface blocked tools: {metadata:?}"
        );
    }

    #[test]
    fn admission_rejections_fail_the_boundary_without_counting_as_execution_errors() {
        let records = vec![ToolCallRecord {
            tool_call_id: Some("call-rejected".into()),
            name: "list_dir".into(),
            ok: false,
            error: Some("canonical Work admission unavailable".into()),
            error_kind: Some(astra_core::ErrorKind::ContractViolation),
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            ..Default::default()
        }];

        let eval =
            evaluate_tool_call_records("inspect the workspace", &[], &records, 0, false, 0.0);

        assert!(!eval.success);
        assert!(
            eval.signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::BlockedToolCall { count: 1 })),
            "typed admission rejection must be visible to evaluation: {:?}",
            eval.signals
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::ToolErrorRate(rate) if (*rate - 1.0).abs() < f64::EPSILON)),
            "a pre-execution rejection must not become an executor error: {:?}",
            eval.signals
        );
    }

    #[test]
    fn one_blocked_route_cannot_hide_behind_many_successful_observations() {
        let mut records = (0..7)
            .map(|index| journal_ok_call(&format!("read_{index}")))
            .collect::<Vec<_>>();
        records.push(ToolCallRecord {
            tool_call_id: Some("required-route".into()),
            name: "bash".into(),
            ok: false,
            error: Some("policy rejected the required route".into()),
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            ..Default::default()
        });

        let eval = evaluate_tool_call_records("diagnose the failure", &[], &records, 0, false, 0.0);
        assert!(
            !eval.success,
            "successful side observations must not erase an unresolved blocked route: {eval:?}"
        );
        assert!(
            eval.signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::BlockedToolCall { count: 1 }))
        );
    }

    #[test]
    fn successful_retry_resolves_the_same_blocked_operation() {
        let args = serde_json::json!({"command": "custom-inspector --status"}).to_string();
        let mut blocked = ToolCallRecord {
            tool_call_id: Some("blocked-call".into()),
            name: "bash".into(),
            ok: false,
            error: Some("approval required".into()),
            args_full: Some(args.clone()),
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            ..Default::default()
        };
        let mut recovered = journal_ok_call("bash");
        recovered.tool_call_id = Some("retry-call".into());
        recovered.args_full = Some(args);
        recovered.disposition =
            Some(astra_services::session_journal::ToolCallDisposition::Executed);

        let eval = evaluate_tool_call_records(
            "inspect the environment",
            &[],
            &[blocked.clone(), recovered],
            0,
            false,
            0.0,
        );
        assert!(
            eval.success,
            "an authoritative same-operation retry resolves the block: {eval:?}"
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::BlockedToolCall { .. }))
        );

        blocked.args_full = Some(serde_json::json!({"command": "other-command"}).to_string());
        let unresolved = evaluate_tool_call_records(
            "inspect the environment",
            &[],
            &[blocked, journal_ok_call("bash")],
            0,
            false,
            0.0,
        );
        assert!(!unresolved.success);
    }

    fn journal_ok_call(name: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: None,
            result_preview: Some("ok".to_string()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn unclassified_failed_execution_is_feedback_and_matching_success_resolves_it() {
        let args = serde_json::json!({"command": "cargo test"}).to_string();
        let mut failed = journal_ok_call("bash");
        failed.ok = false;
        failed.args_full = Some(args.clone());
        failed.result_class = None;

        assert_eq!(
            count_unresolved_tool_outcome_failures(std::slice::from_ref(&failed)),
            1,
            "a governed failure must remain actionable even without optional classification"
        );

        let mut recovered = journal_ok_call("bash");
        recovered.args_full = Some(args);
        recovered.result_class = None;
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed, recovered]),
            0,
            "a later successful execution of the same operation resolves the fallback failure"
        );
    }

    #[test]
    fn domain_negative_exit_does_not_create_unresolved_obligation() {
        let mut record = journal_ok_call("bash");
        record.ok = false;
        record.args_full = Some(serde_json::json!({"command": "grep missing file"}).to_string());
        record.exit_semantics = Some("domain_negative".to_string());
        assert_eq!(count_unresolved_tool_outcome_failures(&[record.clone()]), 0);
        assert!(active_execution_failure_operation_keys(&[record]).is_empty());
    }

    #[test]
    fn rejected_typed_agent_result_is_not_an_unresolved_execution() {
        let mut record = journal_ok_call("agent");
        record.ok = false;
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        record.args_full =
            Some(serde_json::json!({"action":"spawn","prompt":"review"}).to_string());
        record.result_class = Some("agent_incomplete".to_string());

        assert_eq!(
            count_unresolved_tool_outcome_failures(std::slice::from_ref(&record)),
            0
        );
        assert!(active_execution_failure_operation_keys(&[record]).is_empty());
    }

    #[test]
    fn positive_success_is_stricter_than_completed_domain_outcome() {
        let mut record = journal_ok_call("bash");
        record.args_full =
            Some(serde_json::json!({"command": "cargo test | grep PASSED"}).to_string());
        record.result_class = Some("empty_result".to_string());
        record.exit_semantics = Some("empty_result".to_string());
        assert!(record.ok, "the shell transport itself completed");
        assert!(!tool_outcome_is_positive_success(&record));

        record.result_class = Some("success".to_string());
        record.exit_semantics = Some("success".to_string());
        assert!(tool_outcome_is_positive_success(&record));

        record.exit_semantics = Some("pipeline_truncated".to_string());
        assert!(
            !tool_outcome_is_positive_success(&record),
            "a truncated pipeline is not an affirmative validation receipt"
        );

        record.exit_semantics = None;
        assert!(
            !tool_outcome_is_positive_success(&record),
            "missing process semantics must remain fail-closed"
        );
    }

    #[test]
    fn typed_failure_overrides_domain_negative_process_semantics() {
        let mut record = journal_ok_call("bash");
        record.ok = false;
        record.args_full = Some(serde_json::json!({"command": "cargo test"}).to_string());
        record.result_class = Some("test_failure".to_string());
        record.exit_semantics = Some("domain_negative".to_string());
        assert_eq!(count_unresolved_tool_outcome_failures(&[record.clone()]), 1);
        assert_eq!(active_execution_failure_operation_keys(&[record]).len(), 1);
    }

    #[test]
    fn unclassified_failure_without_identity_is_not_erased_by_same_named_success() {
        let mut failed = journal_ok_call("bash");
        failed.ok = false;
        failed.result_class = None;

        let recovered = journal_ok_call("bash");
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed, recovered]),
            1,
            "without an operation identity, an unrelated success must not erase feedback"
        );

        let mut failed_with_id = journal_ok_call("bash");
        failed_with_id.ok = false;
        failed_with_id.result_class = None;
        failed_with_id.tool_call_id = Some("call-1".to_string());
        let mut recovered_with_id = journal_ok_call("bash");
        recovered_with_id.tool_call_id = Some("call-1".to_string());
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed_with_id, recovered_with_id]),
            0,
            "a shared provider call identity is sufficient to resolve the fallback failure"
        );

        let args = serde_json::json!({"command": "cargo test"}).to_string();
        let mut failed_retry = journal_ok_call("bash");
        failed_retry.ok = false;
        failed_retry.args_full = Some(args.clone());
        failed_retry.tool_call_id = Some("failed-attempt".to_string());
        let mut recovered_retry = journal_ok_call("bash");
        recovered_retry.args_full = Some(args);
        recovered_retry.tool_call_id = Some("new-attempt".to_string());
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed_retry, recovered_retry]),
            0,
            "stable operation arguments must resolve a retry even when provider ids rotate"
        );
    }

    #[test]
    fn active_operation_projection_matches_terminal_recovery_identity() {
        let mut failed = journal_ok_call("bash");
        failed.ok = false;
        failed.args_full = Some(serde_json::json!({"command":"cargo test"}).to_string());
        failed.result_class = Some("test_failure".into());

        let mut unrelated_success = journal_ok_call("bash");
        unrelated_success.args_full =
            Some(serde_json::json!({"command":"cargo build"}).to_string());
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed.clone(), unrelated_success]),
            1
        );

        let mut matching_success = journal_ok_call("bash");
        matching_success.args_full = Some(serde_json::json!({"command":"cargo test"}).to_string());
        assert!(
            active_execution_failure_operation_keys(&[failed, matching_success.clone()]).is_empty()
        );
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[
                {
                    let mut f = journal_ok_call("bash");
                    f.ok = false;
                    f.args_full = Some(serde_json::json!({"command":"cargo test"}).to_string());
                    f.result_class = Some("test_failure".into());
                    f
                },
                matching_success,
            ]),
            0
        );
    }

    #[test]
    fn operation_identity_is_stable_across_result_shape_and_json_key_order() {
        let mut typed_failure = journal_ok_call("bash");
        typed_failure.ok = false;
        typed_failure.result_class = Some("execution_error".into());
        typed_failure.args_full = Some(r#"{"command":"probe","options":{"z":1,"a":2}}"#.into());

        let mut untyped_retry = journal_ok_call("bash");
        untyped_retry.result_class = None;
        untyped_retry.args_full = Some(r#"{"options":{"a":2,"z":1},"command":"probe"}"#.into());

        assert_eq!(
            tool_outcome_operation_key(&typed_failure),
            tool_outcome_operation_key(&untyped_retry),
            "result classification and JSON object ordering must not change operation identity"
        );
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[typed_failure, untyped_retry]),
            0,
            "a successful untyped retry must resolve a typed failure for the same operation"
        );
    }

    #[test]
    fn operation_identity_keeps_distinct_arguments_isolated() {
        let mut first = journal_ok_call("bash");
        first.ok = false;
        first.result_class = Some("execution_error".into());
        first.args_full = Some(r#"{"command":"probe-a"}"#.into());

        let mut different = journal_ok_call("bash");
        different.args_full = Some(r#"{"command":"probe-b"}"#.into());

        assert_ne!(
            tool_outcome_operation_key(&first),
            tool_outcome_operation_key(&different)
        );
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[first, different]),
            1,
            "a different operation must not clear the active failure"
        );
    }

    #[test]
    fn rejected_operation_identity_is_cleared_by_canonical_success() {
        let mut rejected = journal_ok_call("bash");
        rejected.ok = false;
        rejected.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        rejected.args_full = Some(r#"{"command":"probe","options":{"z":1,"a":2}}"#.into());

        let mut recovered = journal_ok_call("bash");
        recovered.args_full = Some(r#"{"options":{"a":2,"z":1},"command":"probe"}"#.into());

        assert!(active_rejected_operation_keys(&[rejected, recovered]).is_empty());
        assert_eq!(
            unresolved_blocked_attempt_count(&[
                {
                    let mut record = journal_ok_call("bash");
                    record.ok = false;
                    record.disposition =
                        Some(astra_services::session_journal::ToolCallDisposition::Rejected);
                    record.args_full =
                        Some(r#"{"command":"probe","options":{"z":1,"a":2}}"#.into());
                    record
                },
                {
                    let mut record = journal_ok_call("bash");
                    record.args_full =
                        Some(r#"{"options":{"a":2,"z":1},"command":"probe"}"#.into());
                    record
                },
            ]),
            0
        );
    }

    // ── evaluate_turn quality levels ──

    #[test]
    fn evaluate_turn_success_and_failure_quality() {
        // all tools succeed → high quality
        let all_ok = evaluate_turn(
            &[ok_call("bash"), ok_call("grep"), ok_call("read_file")],
            0,
            false,
            0.3,
            false,
        );
        assert!(all_ok.success);
        assert!(all_ok.quality > 0.7);
        assert!(all_ok.signals.contains(&EvalSignal::AllToolsHealthy));

        // all tools fail → low quality
        let all_err = evaluate_turn(&[err_call("bash"), err_call("grep")], 0, false, 0.3, false);
        assert!(!all_err.success);
        assert!(all_err.quality < 0.4);
        assert!(
            all_err
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::ToolErrorRate(r) if *r > 0.9))
        );

        // mixed success → evidence may remain, but the turn verdict is not healthy
        let mixed = evaluate_turn(
            &[ok_call("bash"), err_call("grep"), ok_call("read_file")],
            0,
            false,
            0.3,
            false,
        );
        assert!(
            !mixed.success,
            "any real tool error must prevent a healthy success verdict"
        );
        assert!(
            !mixed.signals.contains(&EvalSignal::AllToolsHealthy),
            "partial failure must not be labelled all-tools-healthy"
        );
        assert!(
            mixed
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::ToolErrorRate(_)))
        );
    }

    #[test]
    fn unresolved_execution_failure_is_distinct_from_generic_low_quality() {
        let low_quality = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.8,
            signals: vec![EvalSignal::VerdictWarning],
            thresholds: EvaluationThresholds::default(),
        };
        assert!(!turn_evaluation_has_unresolved_execution_failure(
            &low_quality
        ));

        let execution_incomplete = TurnEvaluation {
            signals: vec![EvalSignal::ToolOutcomeFailure {
                class: "transport_unavailable".to_string(),
                count: 2,
            }],
            ..low_quality.clone()
        };
        assert!(turn_evaluation_has_unresolved_execution_failure(
            &execution_incomplete
        ));

        let recovered_with_partial_evidence = TurnEvaluation {
            signals: vec![
                EvalSignal::ToolErrorRate(0.01),
                EvalSignal::ToolOutcomeFailure {
                    class: "execution_error".to_string(),
                    count: 1,
                },
            ],
            ..low_quality.clone()
        };
        assert!(
            !turn_evaluation_has_unresolved_execution_failure(&recovered_with_partial_evidence),
            "a minority optional-probe failure is advisory evidence, not a failed child lifecycle"
        );

        let nested_agent_incomplete = TurnEvaluation {
            signals: vec![
                EvalSignal::ToolErrorRate(0.01),
                EvalSignal::ToolOutcomeFailure {
                    class: RESULT_CLASS_AGENT_INCOMPLETE.to_string(),
                    count: 1,
                },
            ],
            ..low_quality
        };
        assert!(turn_evaluation_has_unresolved_execution_failure(
            &nested_agent_incomplete
        ));
    }

    #[test]
    fn low_tool_error_rate_is_not_successful() {
        let eval = evaluate_turn(
            &[
                ok_call("web_fetch"),
                ok_call("web_fetch"),
                ok_call("run_script"),
                ok_call("agent"),
                err_call("web_fetch"),
            ],
            0,
            false,
            0.2,
            false,
        );

        assert!(
            !eval.success,
            "one failed tool in an otherwise productive turn still needs visible diagnosis"
        );
        assert!(
            eval.signals.iter().any(
                |s| matches!(s, EvalSignal::ToolErrorRate(rate) if *rate > 0.0 && *rate < 0.5)
            ),
            "the evaluator should retain the partial-failure rate signal"
        );
        assert!(!eval.signals.contains(&EvalSignal::AllToolsHealthy));
    }

    #[test]
    fn no_tools_conversational_vs_factual() {
        // conversational: no tools needed, moderate quality, low confidence
        let conv = evaluate_turn(&[], 0, false, 0.3, false);
        assert!(conv.success);
        assert_eq!(conv.quality, 0.5);
        assert!(conv.confidence < 0.5);

        // factual query with no tools: flagged as bad
        let factual = evaluate_turn(&[], 0, false, 0.3, true);
        assert!(!factual.success);
        assert!(factual.quality < 0.3);
        assert!(factual.signals.contains(&EvalSignal::NoToolsNeeded));
    }

    #[test]
    fn signal_detection_and_penalties() {
        let base = evaluate_turn(&[ok_call("bash")], 0, false, 0.3, false);

        // stalls reduce quality
        let stall = evaluate_turn(&[ok_call("bash")], 2, false, 0.3, false);
        assert!(stall.quality < base.quality);
        assert!(stall.signals.contains(&EvalSignal::StallDetected));

        // verdict warning reduces quality
        let verdict = evaluate_turn(&[ok_call("bash")], 0, true, 0.3, false);
        assert!(verdict.quality < base.quality);
        assert!(verdict.signals.contains(&EvalSignal::VerdictWarning));

        // high budget pressure penalizes
        let pressure = evaluate_turn(&[ok_call("bash")], 0, false, 0.85, false);
        assert!(pressure.quality < base.quality);
        assert!(pressure.signals.contains(&EvalSignal::HighBudgetPressure));

        // repeat tool calls detected
        let repeat = evaluate_turn(
            &[
                ok_call("bash"),
                ok_call("bash"),
                ok_call("bash"),
                ok_call("grep"),
            ],
            0,
            false,
            0.3,
            false,
        );
        assert!(
            repeat
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(n) if n == "bash"))
        );

        // empty output penalizes more than all-ok
        let empty = evaluate_turn(
            &[empty_call("read_file"), ok_call("bash")],
            0,
            false,
            0.3,
            false,
        );
        assert!(empty.signals.contains(&EvalSignal::EmptyToolOutput));
        assert!(empty.quality < base.quality);

        // quality clamped to [0, 1]
        let worst = evaluate_turn(
            &[err_call("a"), err_call("b"), err_call("c")],
            5,
            true,
            0.9,
            true,
        );
        assert!(worst.quality >= 0.0 && worst.quality <= 1.0);
        assert!(worst.confidence >= 0.0 && worst.confidence <= 1.0);

        // confidence increases with more signals
        let complex = evaluate_turn(&[err_call("bash"), err_call("grep")], 2, true, 0.9, false);
        assert!(complex.confidence > base.confidence);
    }

    #[test]
    fn noop_tool_results_are_low_yield_not_all_tools_healthy() {
        let eval = evaluate_turn(&[noop_call("read_file")], 0, false, 0.3, false);

        assert!(eval.signals.contains(&EvalSignal::NoOpToolResults(1)));
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::AllToolsHealthy)),
            "{:?}",
            eval.signals
        );
        assert!(eval.success);
        assert!(eval.quality < 0.5, "{eval:?}");
    }

    #[test]
    fn evaluate_tool_call_records_does_not_infer_live_query_from_text() {
        let eval = evaluate_tool_call_records(
            "Check the latest git status",
            &["git".to_string()],
            &[],
            0,
            false,
            0.2,
        );
        assert!(eval.success);
        assert!(!eval.signals.contains(&EvalSignal::NoToolsNeeded));
    }

    #[test]
    fn evaluate_tool_call_records_treats_validated_cache_reuse_as_delivered_evidence() {
        let mut record = journal_ok_call("read_file");
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Reused);
        record.result_preview = Some("reused file contents".to_string());

        let eval = evaluate_tool_call_records("Summarize file", &[], &[record], 0, false, 0.2);

        assert!(
            eval.signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::AllToolsHealthy)),
            "{:?}",
            eval.signals
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::NoOpToolResults(_))),
            "{:?}",
            eval.signals
        );
    }

    #[test]
    fn evaluate_tool_call_records_counts_suppressed_result_as_noop() {
        let mut record = journal_ok_call("read_file");
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Suppressed);

        let eval = evaluate_tool_call_records("Summarize file", &[], &[record], 0, false, 0.2);

        assert!(eval.signals.contains(&EvalSignal::NoOpToolResults(1)));
    }

    #[test]
    fn evaluate_tool_call_records_ignores_noop_human_text_without_structured_signal() {
        let mut record = journal_ok_call("read_file");
        record.result_preview = Some("[cached_cross_turn: reused 200 bytes]".to_string());

        let eval = evaluate_tool_call_records("Summarize file", &[], &[record], 0, false, 0.2);

        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::NoOpToolResults(_))),
            "{:?}",
            eval.signals
        );
    }

    #[test]
    fn online_progress_continues_when_signal_is_below_threshold() {
        let decision = decide_online_progress(
            OnlineProgressSignals {
                tool_calls: ONLINE_PROGRESS_MIN_TOOL_CALLS_BEFORE_NUDGE,
                redundant_overlapping_reads: REDUNDANT_OVERLAPPING_READS_THRESHOLD - 1,
                ..OnlineProgressSignals::default()
            },
            OnlineProgressPolicy::default(),
        );

        assert_eq!(decision, OnlineProgressDecision::Continue);
    }

    #[test]
    fn online_progress_requires_minimum_observation_volume() {
        let decision = decide_online_progress(
            OnlineProgressSignals {
                tool_calls: ONLINE_PROGRESS_MIN_TOOL_CALLS_BEFORE_NUDGE - 1,
                redundant_overlapping_reads: REDUNDANT_OVERLAPPING_READS_THRESHOLD,
                ..OnlineProgressSignals::default()
            },
            OnlineProgressPolicy::default(),
        );

        assert_eq!(decision, OnlineProgressDecision::Continue);
    }

    #[test]
    fn online_progress_nudges_on_structured_redundant_reads_only_after_threshold() {
        let decision = decide_online_progress(
            OnlineProgressSignals {
                tool_calls: ONLINE_PROGRESS_MIN_TOOL_CALLS_BEFORE_NUDGE,
                redundant_overlapping_reads: REDUNDANT_OVERLAPPING_READS_THRESHOLD,
                ..OnlineProgressSignals::default()
            },
            OnlineProgressPolicy::default(),
        );

        assert_eq!(
            decision,
            OnlineProgressDecision::ReuseKnownContext {
                redundant_overlapping_reads: REDUNDANT_OVERLAPPING_READS_THRESHOLD
            }
        );
    }

    #[test]
    fn online_progress_does_not_stack_on_existing_intervention() {
        let decision = decide_online_progress(
            OnlineProgressSignals {
                tool_calls: 100,
                redundant_overlapping_reads: 100,
                stronger_advisory_emitted: true,
                ..OnlineProgressSignals::default()
            },
            OnlineProgressPolicy::default(),
        );

        assert_eq!(decision, OnlineProgressDecision::Continue);
    }

    #[test]
    fn evaluate_tool_call_records_marks_unresolved_outcome_failure_unsuccessful() {
        let mut record = journal_ok_call("bash");
        record.args_full = Some(
            serde_json::json!({"command": "cargo test -p astra-runtime 2>&1 | tail -20"})
                .to_string(),
        );
        record.result_class = Some("test_failure".to_string());
        record.exit_semantics = Some("domain_negative".to_string());

        let eval = evaluate_tool_call_records("run the tests", &[], &[record], 0, false, 0.2);

        assert!(!eval.success, "{eval:?}");
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::AllToolsHealthy)),
            "{:?}",
            eval.signals
        );
        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::ToolOutcomeFailure { class, count }
                if class == "test_failure" && *count == 1
        )));
        let json = eval_signals_to_json_with_thresholds(&eval.signals, eval.thresholds);
        assert!(
            json.iter()
                .any(|signal| signal["kind"] == "tool_outcome_failure"
                    && signal["class"] == "test_failure")
        );
    }

    #[test]
    fn rejected_runtime_route_remains_an_unresolved_execution_failure() {
        let mut record = journal_ok_call("web_fetch");
        record.args_full = Some(serde_json::json!({"url": "https://news.example/"}).to_string());
        record.ok = false;
        record.result_class = Some("execution_error".to_string());
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);

        let eval =
            evaluate_tool_call_records("fetch a current headline", &[], &[record], 0, false, 0.2);

        assert!(!eval.success, "{eval:?}");
        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::ToolOutcomeFailure { class, count }
                if class == "execution_error" && *count == 1
        )));
        assert!(turn_evaluation_has_unresolved_execution_failure(&eval));
    }

    #[test]
    fn minority_optional_probe_failure_is_not_unresolved_execution_evidence() {
        let mut failed_probe = journal_ok_call("bash");
        failed_probe.args_full =
            Some(serde_json::json!({"command": "optional-environment-probe"}).to_string());
        failed_probe.ok = false;
        failed_probe.result_class = Some("execution_error".to_string());

        let successful_reads = ["src/lib.rs", "src/runtime.rs", "tests/e2e.rs"]
            .into_iter()
            .map(|path| {
                let mut record = journal_ok_call("read_file");
                record.args_full = Some(serde_json::json!({"path": path}).to_string());
                record
            })
            .collect::<Vec<_>>();
        let mut records = vec![failed_probe];
        records.extend(successful_reads);

        let eval =
            evaluate_tool_call_records("review the implementation", &[], &records, 0, false, 0.2);

        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::ToolOutcomeFailureCoverage {
                unresolved: 1,
                observed: 4
            }
        )));
        assert!(
            !turn_evaluation_has_unresolved_execution_failure(&eval),
            "the failed probe remains advisory evidence without erasing the productive evidence"
        );
    }

    #[test]
    fn successful_fallback_preserves_failure_evidence_without_claiming_health() {
        let mut rejected_route = journal_ok_call("bash");
        rejected_route.args_full =
            Some(serde_json::json!({"command": "curl https://news.example/"}).to_string());
        rejected_route.ok = false;
        rejected_route.result_class = Some("execution_error".to_string());
        rejected_route.disposition =
            Some(astra_services::session_journal::ToolCallDisposition::Rejected);

        let mut successful_fallback = journal_ok_call("web_fetch");
        successful_fallback.args_full =
            Some(serde_json::json!({"url": "https://news.example/"}).to_string());

        let eval = evaluate_tool_call_records(
            "fetch one current headline",
            &[],
            &[rejected_route, successful_fallback],
            0,
            false,
            0.2,
        );

        assert!(
            !eval.success,
            "the rejected route remains visible: {eval:?}"
        );
        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::ToolOutcomeFailureCoverage {
                unresolved: 1,
                observed: 2
            }
        )));
        assert!(
            eval.signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::BlockedToolCall { count: 1 }))
        );
    }

    #[test]
    fn terminal_rejection_boundary_does_not_reopen_after_a_later_execution() {
        let mut rejected = journal_ok_call("bash");
        rejected.ok = false;
        rejected.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        let recovered = journal_ok_call("read_file");

        assert_eq!(terminal_rejected_attempt_count(&[rejected.clone()]), 1);
        assert_eq!(
            terminal_rejected_attempt_count(&[rejected, recovered]),
            0,
            "a real later execution is already a recovery boundary"
        );
    }

    #[test]
    fn evaluate_tool_call_records_marks_structured_agent_interruptions_unsuccessful() {
        let mut interrupted = journal_ok_call("agent");
        interrupted.args_full = Some(
            serde_json::json!({"action": "get_result", "agent_id": "reviewer@abc"}).to_string(),
        );
        interrupted.result_full = Some(
            serde_json::json!({
                "status": "interrupted",
                "agent_id": "reviewer@abc",
                "finish_reason": "empty_completion",
                "incomplete": true,
                "result": "partial review"
            })
            .to_string(),
        );

        let eval =
            evaluate_tool_call_records("review the branch", &[], &[interrupted], 0, false, 0.2);

        assert!(!eval.success, "{eval:?}");
        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::ToolOutcomeFailure { class, count }
                if class == RESULT_CLASS_AGENT_INCOMPLETE && *count == 1
        )));
    }

    #[test]
    fn evaluate_tool_call_records_marks_structured_active_agent_as_incomplete() {
        let mut active = journal_ok_call("agent");
        active.args_full = Some(
            serde_json::json!({"action": "get_result", "agent_id": "reviewer@abc"}).to_string(),
        );
        active.result_full = Some(
            serde_json::json!({
                "status": "still_running",
                "agent_id": "reviewer@abc"
            })
            .to_string(),
        );

        let eval =
            evaluate_tool_call_records("collect child result", &[], &[active], 0, false, 0.2);

        assert!(!eval.success, "{eval:?}");
        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::ToolOutcomeFailure { class, count }
                if class == RESULT_CLASS_AGENT_INCOMPLETE && *count == 1
        )));
    }

    #[test]
    fn later_structured_agent_completion_resolves_prior_interruption() {
        let args =
            serde_json::json!({"action": "get_result", "agent_id": "reviewer@abc"}).to_string();
        let mut interrupted = journal_ok_call("agent");
        interrupted.args_full = Some(args.clone());
        interrupted.result_full = Some(
            serde_json::json!({
                "status": "interrupted",
                "agent_id": "reviewer@abc",
                "incomplete": true
            })
            .to_string(),
        );
        let mut completed = journal_ok_call("agent");
        completed.args_full = Some(args);
        completed.result_full = Some(
            serde_json::json!({
                "status": "completed",
                "agent_id": "reviewer@abc",
                "result": "done"
            })
            .to_string(),
        );

        let eval = evaluate_tool_call_records(
            "continue the child agent",
            &[],
            &[interrupted, completed],
            0,
            false,
            0.2,
        );

        assert!(eval.success, "{eval:?}");
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::ToolOutcomeFailure { .. })),
            "{:?}",
            eval.signals
        );
    }

    #[test]
    fn evaluate_tool_call_records_marks_structured_fanout_issues_unsuccessful() {
        let mut fanout = journal_ok_call("agent_fanout");
        fanout.args_full =
            Some(serde_json::json!({"action": "get_results", "group_id": "review"}).to_string());
        fanout.result_full = Some(
            serde_json::json!({
                "status": "completed_with_issues",
                "group_id": "review",
                "interrupted": 1,
                "results": [{
                    "slot_index": 0,
                    "agent_id": "reviewer@abc",
                    "result": {
                        "status": "interrupted",
                        "agent_id": "reviewer@abc",
                        "incomplete": true
                    }
                }]
            })
            .to_string(),
        );

        let eval = evaluate_tool_call_records("review the branch", &[], &[fanout], 0, false, 0.2);

        assert!(!eval.success, "{eval:?}");
        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::ToolOutcomeFailure { class, count }
                if class == RESULT_CLASS_FANOUT_INCOMPLETE && *count == 1
        )));
    }

    #[test]
    fn evaluate_tool_call_records_marks_structured_active_fanout_as_incomplete() {
        let mut fanout = journal_ok_call("agent_fanout");
        fanout.args_full =
            Some(serde_json::json!({"action": "get_results", "group_id": "review"}).to_string());
        fanout.result_full = Some(
            serde_json::json!({
                "status": "incomplete",
                "group_id": "review",
                "results": [{
                    "slot_index": 0,
                    "agent_id": "reviewer@abc",
                    "result": {
                        "status": "still_running",
                        "agent_id": "reviewer@abc"
                    }
                }]
            })
            .to_string(),
        );

        let eval =
            evaluate_tool_call_records("collect fanout results", &[], &[fanout], 0, false, 0.2);

        assert!(!eval.success, "{eval:?}");
        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::ToolOutcomeFailure { class, count }
                if class == RESULT_CLASS_FANOUT_INCOMPLETE && *count == 1
        )));
    }

    #[test]
    fn structured_fanout_journal_record_produces_actionable_incomplete_notice() {
        let mut fanout = journal_ok_call("agent_fanout");
        fanout.args_full =
            Some(serde_json::json!({"action": "get_results", "group_id": "review"}).to_string());
        fanout.result_full = Some(
            serde_json::json!({
                "status": "incomplete",
                "group_id": "review",
                "recovery": {
                    "resume_existing_work_before_rerun": true,
                    "rerun_policy": "resume_existing_agents_or_report_incomplete; do_not_respawn_slots"
                },
                "results": [{
                    "slot_index": 0,
                    "agent_id": "reviewer@abc",
                    "result": {
                        "status": "still_running",
                        "agent_id": "reviewer@abc"
                    },
                    "recovery": {
                        "resume_existing_agent_id": "reviewer@abc",
                        "rerun_policy": "resume_existing_agent_or_report_incomplete",
                        "do_not_spawn_replacement": true
                    }
                }]
            })
            .to_string(),
        );

        let eval = evaluate_tool_call_records(
            "collect fanout results",
            &[],
            std::slice::from_ref(&fanout),
            0,
            false,
            0.2,
        );
        let event = build_turn_evaluation_journal_event(
            Some("s1"),
            Some(1),
            "turn",
            "collect fanout results",
            &[],
            &[fanout],
            0,
            false,
            0.2,
            &eval,
        );

        assert!(!eval.success, "{eval:?}");
        assert_eq!(
            turn_evaluation_status_notice(&eval).as_deref(),
            Some(
                "Turn finished with unresolved tool/runtime failure(s): fanout_incomplete x1. Treat the final answer as incomplete until validation passes or the provider surface changes."
            )
        );
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["success"], false);
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["signals"][1]["kind"], "tool_outcome_failure");
        assert_eq!(
            metadata["signals"][1]["class"],
            RESULT_CLASS_FANOUT_INCOMPLETE
        );
    }

    #[test]
    fn later_matching_success_resolves_tool_outcome_failure() {
        let mut failed = journal_ok_call("bash");
        failed.args_full = Some(
            serde_json::json!({"command": "cargo test -p astra-runtime | tail -20"}).to_string(),
        );
        failed.result_class = Some("test_failure".to_string());
        failed.exit_semantics = Some("domain_negative".to_string());

        let mut passed = journal_ok_call("bash");
        passed.args_full =
            Some(serde_json::json!({"command": "cargo test -p astra-runtime"}).to_string());
        passed.result_class = Some("success".to_string());
        passed.exit_semantics = Some("success".to_string());

        let eval = evaluate_tool_call_records(
            "fix and rerun the tests",
            &[],
            &[failed, passed],
            0,
            false,
            0.2,
        );

        assert!(eval.success, "{eval:?}");
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::ToolOutcomeFailure { .. }))
        );
    }

    #[test]
    fn build_turn_evaluation_journal_event_serializes_normalized_signals() {
        let records = vec![journal_ok_call("git")];
        let eval = evaluate_tool_call_records(
            "Check the latest git status",
            &["git".to_string()],
            &records,
            0,
            false,
            0.2,
        );

        let event = build_turn_evaluation_journal_event(
            Some("sess-1"),
            Some(2),
            "cli_repl",
            "Check the latest git status",
            &["git".to_string()],
            &records,
            0,
            false,
            0.2,
            &eval,
        );

        assert_eq!(event.event_type, JournalEventType::TurnEvaluation);
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["source"], "cli_repl");
        assert_eq!(metadata["live_query"], false);
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["tool_execution_count"], 1);
        assert_eq!(metadata["tool_rejected_count"], 0);
        assert_eq!(metadata["signal_count"], 2);
        assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
        assert_eq!(metadata["signals"][1]["kind"], "all_tools_healthy");
    }

    #[test]
    fn web_tool_records_mark_live_queries_without_parsing_user_text() {
        let records = vec![journal_ok_call("web_search")];
        let eval = evaluate_tool_call_records("试一试", &[], &records, 0, false, 0.2);
        let event = build_turn_evaluation_journal_event(
            Some("sess-live"),
            Some(3),
            "server_runtime",
            "试一试",
            &[],
            &records,
            0,
            false,
            0.2,
            &eval,
        );
        let metadata = event.metadata.expect("turn evaluation metadata");

        assert_eq!(metadata["live_query"], true);
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["tool_execution_count"], 1);
        assert_eq!(metadata["tool_rejected_count"], 0);
    }

    #[test]
    fn evaluate_records_skips_synthetic_placeholders() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        let rec = |name: &str, ok: bool, preview: Option<&str>| ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: preview.map(ToString::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        // 1 real successful tool + 3 synthetic placeholders (skipped,
        // deferred, surgically_removed). A naive counter would see
        // error_rate = 3/4 = 0.75 and quality collapse. With filtering,
        // only the real success is counted and the turn scores as healthy.
        let mut surgical = rec(
            SURGICAL_REMOVAL_TOOL_NAME,
            true,
            Some("(removed from context — skill covered this work)"),
        );
        surgical.surgically_removed = Some(true);

        let mut skipped = rec("read_file", true, Some("Skipped: skill routed"));
        skipped.result_class =
            Some(astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS.to_string());
        skipped.skill_reentry_count = Some(1);

        let mut deferred = rec("read_file", true, Some("Deferred: skill invoked"));
        deferred.result_class =
            Some(astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS.to_string());
        deferred.skill_locked_out = Some(true);

        let records = vec![
            rec("git", true, Some("diff contents here")),
            surgical,
            skipped,
            deferred,
        ];

        let eval = evaluate_tool_call_records(
            "review commit 179afcb",
            &["git".to_string()],
            &records,
            0,
            false,
            0.1,
        );

        // Must contain exactly one error_rate=0.0 signal AND AllToolsHealthy
        // — proves surgical/skipped/deferred were filtered before analysis.
        let has_zero_error = eval.signals.iter().any(
            |s| matches!(s, EvalSignal::ToolErrorRate(rate) if (*rate - 0.0).abs() < f64::EPSILON),
        );
        assert!(
            has_zero_error,
            "expected ToolErrorRate(0.0) after filtering synthetic placeholders, got {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::AllToolsHealthy)),
            "expected AllToolsHealthy signal, got {:?}",
            eval.signals
        );
        assert!(
            eval.success,
            "turn with one real success and synthetic placeholders must be success=true"
        );
        assert!(
            eval.quality > 0.6,
            "quality should be high after filtering, got {}",
            eval.quality
        );
    }

    #[test]
    fn repeat_tool_call_ignores_surgical_removals() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // Four surgically_removed records in a single turn used to surface
        // as RepeatToolCall("(surgically_removed)") — a false "retry loop"
        // signal. After filtering they must not appear at all.
        let records: Vec<ToolCallRecord> = (0..4)
            .map(|_| ToolCallRecord {
                name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
                ok: true,
                ms: 0,
                error: None,
                input_bytes: None,
                output_bytes: Some(0),
                args_preview: None,
                result_preview: Some("(removed from context — skill covered this work)".into()),
                file_path: None,
                surgically_removed: Some(true),
                original_tool_name: Some("read_file".to_string()),
                ..Default::default()
            })
            .chain(std::iter::once(ToolCallRecord {
                name: "git".to_string(),
                ok: true,
                ms: 20,
                error: None,
                input_bytes: None,
                output_bytes: Some(400),
                args_preview: None,
                result_preview: Some("diff".into()),
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            }))
            .collect();

        let eval = evaluate_tool_call_records("noop", &[], &records, 0, false, 0.1);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(_))),
            "no RepeatToolCall signal should be emitted for synthetic placeholders, got {:?}",
            eval.signals
        );
    }

    #[test]
    fn repeat_tool_call_distinguishes_distinct_args_preview() {
        use astra_services::session_journal::ToolCallRecord;

        let record = |name: &str, args_preview: &str| ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 20,
            error: None,
            input_bytes: None,
            output_bytes: Some(120),
            args_preview: Some(args_preview.to_string()),
            result_preview: Some("ok".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        let distinct_greps = vec![
            record("grep", "/catch_tool_execution_panic/ in ."),
            record("grep", "/CLOUD_APPROVAL_REQUIRED_TOOLS/ in ."),
            record(
                "grep",
                "/struct ToolExecutionOutcome/ in crates/astra-cli/src",
            ),
            record("grep", "/struct ToolExecutionOutcome/ in ."),
        ];
        let eval = evaluate_tool_call_records(
            "review latest commit",
            &["grep".to_string()],
            &distinct_greps,
            0,
            false,
            0.1,
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "grep")),
            "distinct grep queries should not be treated as a retry loop: {:?}",
            eval.signals
        );

        let repeated_git_action_show = vec![
            record("git", r#"{"action":"show","revision":"6f2f96e"}"#),
            record("git", r#"{"action":"show","revision":"6f2f96e"}"#),
            record("git", r#"{"action":"show","revision":"6f2f96e"}"#),
        ];
        let eval = evaluate_tool_call_records(
            "review latest commit",
            &["git".to_string()],
            &repeated_git_action_show,
            0,
            false,
            0.1,
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "git")),
            "identical git(action=show) targets should still surface as repeat loops: {:?}",
            eval.signals
        );
    }

    #[test]
    fn repeat_tool_call_uses_args_full_to_avoid_truncation_collisions() {
        // Reproduces a real false-positive observed in session
        // 24234a1f-1c01-4577-a06a-168e7b583c6d: three legitimate `grep` calls
        // against three distinct files all share the same ~80-char truncated
        // `args_preview` because the long pattern + common path prefix fills
        // the preview before the file name diverges. The repeat-loop signal
        // must look at the *untruncated* `args_full` so distinct calls never
        // collide just because their previews happen to share a prefix.
        use astra_services::session_journal::ToolCallRecord;

        let identical_truncated_preview =
            "grep -n 'canonicalize|unique_path_variants|normalize_path' /workspace/astr…";

        let record = |full_path: &str| ToolCallRecord {
            name: "bash".to_string(),
            ok: true,
            ms: 51,
            error: None,
            input_bytes: None,
            output_bytes: Some(500),
            args_preview: Some(identical_truncated_preview.to_string()),
            args_full: Some(format!(
                r#"{{"command":"grep -n 'canonicalize|unique_path_variants|normalize_path' {}"}}"#,
                full_path
            )),
            result_preview: Some("ok".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        let distinct_full_args = vec![
            record("/workspace/astra/crates/services/src/work.rs"),
            record("/workspace/astra/crates/astra-tools/src/fs_ops.rs"),
            record("/workspace/astra/crates/astra-sandbox/src/policy.rs"),
        ];

        let eval = evaluate_tool_call_records(
            "review canonicalize hot path",
            &["bash".to_string()],
            &distinct_full_args,
            0,
            false,
            0.15,
        );

        assert!(
            !eval.signals.iter().any(|s| matches!(
                s,
                EvalSignal::RepeatToolCall(name) if name == "bash"
            )),
            "three distinct bash invocations differing only past the args_preview \
             truncation must NOT surface as a retry loop: {:?}",
            eval.signals
        );

        // Sanity check: when args_full is genuinely identical, the signal
        // still fires (we did not break legitimate repeat-loop detection).
        let repeated = vec![
            ToolCallRecord {
                name: "bash".into(),
                ok: true,
                ms: 51,
                args_preview: Some(identical_truncated_preview.into()),
                args_full: Some(r#"{"command":"grep -n 'foo' bar.rs"}"#.into()),
                result_preview: Some("ok".into()),
                output_bytes: Some(120),
                ..Default::default()
            };
            3
        ];
        let eval =
            evaluate_tool_call_records("look", &["bash".to_string()], &repeated, 0, false, 0.1);
        assert!(
            eval.signals.iter().any(|s| matches!(
                s,
                EvalSignal::RepeatToolCall(name) if name == "bash"
            )),
            "identical args_full repeats must still surface as a retry loop: {:?}",
            eval.signals
        );
    }

    #[test]
    fn real_session_0ac769_pattern_surfaces_git_action_show_and_read_file_repeats() {
        use astra_services::session_journal::ToolCallRecord;

        let record = |name: &str, args_preview: &str| ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 20,
            error: None,
            input_bytes: None,
            output_bytes: Some(120),
            args_preview: Some(args_preview.to_string()),
            result_preview: Some("ok".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        // Real session 0ac7696c had 7 LLM rounds with this 12-tool pattern:
        // git(action=show), git(action=show), read_file x3 + grep, grep x3 + read_file,
        // git(action=show), git(action=show).
        // The persisted turn_evaluation surfaced repeat loops for read_file and
        // git(action=show), while distinct grep queries stayed healthy.
        let records = vec![
            record(
                "git",
                r#"{"action":"show","revision":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "git",
                r#"{"action":"show","revision":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record("grep", r#"/runtime context/ in crates/runtime/src"#),
            record("grep", r#"/ContinueLoop/ in crates/runtime/src"#),
            record("grep", r#"/TPM/ in crates/runtime/src"#),
            record(
                "read_file",
                r#"{"path":"crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "git",
                r#"{"action":"show","revision":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "git",
                r#"{"action":"show","revision":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record("grep", r#"/turn_evaluation/ in crates/runtime/src"#),
        ];

        let eval = evaluate_tool_call_records(
            "review b273c589a73799070a71f4cfc6d55349b534d8d1",
            &[
                "git".to_string(),
                "read_file".to_string(),
                "grep".to_string(),
            ],
            &records,
            0,
            false,
            0.446_225,
        );

        assert!(
            eval.success,
            "real-session loop still completed successfully"
        );
        assert_eq!(eval.quality, 0.35);
        assert_eq!(eval.confidence, 0.75);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::AllToolsHealthy)),
            "repeat-heavy churn must not still look healthy: {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "read_file")),
            "expected read_file repeat signal, got {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "git")),
            "expected git repeat signal, got {:?}",
            eval.signals
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "grep")),
            "distinct grep queries should not collapse into a repeat loop: {:?}",
            eval.signals
        );

        let event = build_turn_evaluation_journal_event(
            Some("0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0"),
            Some(1),
            "cli_repl",
            "review b273c589a73799070a71f4cfc6d55349b534d8d1",
            &[
                "git".to_string(),
                "read_file".to_string(),
                "grep".to_string(),
            ],
            &records,
            0,
            false,
            0.446_225,
            &eval,
        );
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["tool_call_count"], 12);
        assert_eq!(metadata["signal_count"], 3);
        assert_eq!(metadata["quality"], 0.35);
        assert_eq!(metadata["confidence"], 0.75);
        assert!(
            !metadata["signals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|signal| signal["kind"] == "all_tools_healthy"),
            "turn_evaluation metadata must not keep all_tools_healthy for repeat-heavy churn: {metadata:?}"
        );
    }

    #[test]
    fn round_count_alone_does_not_classify_productive_long_work_as_churn() {
        let records = vec![journal_ok_call("git")];
        let eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
            "review local changes",
            &["git".to_string()],
            &records,
            0,
            false,
            0.26,
            EvaluationThresholds {
                llm_round_churn: 8,
                ..Default::default()
            },
            TurnEvaluationTelemetry {
                llm_rounds: Some(9),
                prompt_tokens: Some(136_947),
                first_round_prompt_tokens: Some(9_401),
                max_round_prompt_tokens: Some(9_401),
            },
        );

        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::LlmRoundChurn { .. })),
            "a long successful turn is not churn without independent low-yield evidence: {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::AllToolsHealthy)),
            "healthy long work should retain its positive execution fact: {:?}",
            eval.signals
        );
    }

    #[test]
    fn llm_round_churn_requires_independent_low_yield_evidence() {
        let records = vec![
            journal_ok_call("git"),
            journal_ok_call("git"),
            journal_ok_call("git"),
        ];
        let eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
            "review local changes",
            &["git".to_string()],
            &records,
            0,
            false,
            0.26,
            EvaluationThresholds {
                llm_round_churn: 8,
                ..Default::default()
            },
            TurnEvaluationTelemetry {
                llm_rounds: Some(9),
                prompt_tokens: Some(136_947),
                first_round_prompt_tokens: Some(9_401),
                max_round_prompt_tokens: Some(9_401),
            },
        );

        assert!(eval.signals.iter().any(|signal| matches!(
            signal,
            EvalSignal::LlmRoundChurn {
                rounds: 9,
                prompt_tokens: 136_947,
            }
        )));
        assert!(
            eval.signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::RepeatToolCall(tool) if tool == "git"))
        );

        let event = build_turn_evaluation_journal_event(
            Some("sess-llm-round"),
            Some(2),
            "cli_repl",
            "review local changes",
            &["git".to_string()],
            &records,
            0,
            false,
            0.26,
            &eval,
        );
        let metadata = event.metadata.expect("turn evaluation metadata");
        let signal = metadata["signals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|signal| signal["kind"] == "llm_round_churn")
            .expect("llm_round_churn signal");
        assert_eq!(signal["rounds"], 9);
        assert_eq!(signal["prompt_tokens"], 136_947);
        assert_eq!(signal["threshold"], 8);
    }

    #[test]
    fn high_cost_low_yield_downgrades_expensive_exploration_churn() {
        let records = vec![
            record_in_round("git", 0, Some("b-0")),
            record_in_round("git", 0, Some("b-0")),
            record_in_round("git", 1, Some("b-1")),
            record_in_round("git", 1, Some("b-1")),
            record_in_round("git", 2, Some("b-2")),
            record_in_round("git", 2, Some("b-2")),
        ];

        let eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
            "review local changes",
            &["git".to_string()],
            &records,
            0,
            false,
            0.26,
            EvaluationThresholds::default(),
            TurnEvaluationTelemetry {
                llm_rounds: Some(9),
                prompt_tokens: Some(136_947),
                first_round_prompt_tokens: Some(9_401),
                max_round_prompt_tokens: Some(20_954),
            },
        );

        assert!(
            eval.signals.iter().any(|signal| matches!(
                signal,
                EvalSignal::HighCostLowYield {
                    tool_calls: 6,
                    llm_rounds: Some(9),
                }
            )),
            "expected high_cost_low_yield signal, got {:?}",
            eval.signals
        );
        assert!(
            !eval.success,
            "expensive low-yield exploration churn should not score as success: {:?}",
            eval
        );
        assert!(
            eval.confidence <= 0.55,
            "confidence should be capped for low-certainty high-cost churn, got {}",
            eval.confidence
        );
    }

    #[test]
    fn high_cost_low_yield_zero_quality_has_low_confidence_cap() {
        assert_eq!(high_cost_low_yield_confidence_cap(0.0), 0.25);
        assert!(
            high_cost_low_yield_confidence_cap(0.35) < 0.50,
            "low quality should not retain high confidence"
        );
        assert_eq!(high_cost_low_yield_confidence_cap(1.0), 0.55);
    }

    #[test]
    fn high_cost_low_yield_confidence_recalibrates_after_late_quality_penalties() {
        let mut records = vec![
            record_in_round("git", 0, Some("b-0")),
            record_in_round("git", 0, Some("b-0")),
            record_in_round("git", 1, Some("b-1")),
            record_in_round("git", 1, Some("b-1")),
            record_in_round("git", 2, Some("b-2")),
            record_in_round("git", 2, Some("b-2")),
        ];
        records[0].result_class = Some("test_failure".to_string());

        let eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
            "review local changes",
            &["git".to_string()],
            &records,
            0,
            false,
            0.95,
            EvaluationThresholds::default(),
            TurnEvaluationTelemetry {
                llm_rounds: Some(9),
                prompt_tokens: Some(136_947),
                first_round_prompt_tokens: Some(9_401),
                max_round_prompt_tokens: Some(20_954),
            },
        );

        assert!(
            eval.quality <= 0.10,
            "late failure penalties must leave high-cost low-yield work with a very low quality score: {eval:?}"
        );
        assert!(
            eval.confidence <= high_cost_low_yield_confidence_cap(eval.quality),
            "high-cost low-yield confidence must stay calibrated after later penalties: {eval:?}"
        );
    }

    #[test]
    fn prompt_growth_churn_does_not_fire_when_first_prompt_tokens_is_zero() {
        let records = vec![record_in_round("bash", 0, Some("b-0"))];
        let eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
            "review local changes",
            &["bash".to_string()],
            &records,
            0,
            false,
            0.26,
            EvaluationThresholds::default(),
            TurnEvaluationTelemetry {
                llm_rounds: Some(6),
                prompt_tokens: Some(50_000),
                first_round_prompt_tokens: Some(0),
                max_round_prompt_tokens: Some(20_000),
            },
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::PromptGrowthChurn { .. })),
            "PromptGrowthChurn must not fire when first_prompt_tokens=0: {:?}",
            eval.signals
        );
    }

    #[test]
    fn high_cost_low_yield_stays_silent_on_high_cost_progressive_turn() {
        let records = vec![
            record_in_round("grep", 0, Some("b-0")),
            record_in_round("grep", 0, Some("b-0")),
            record_in_round("read_file", 1, Some("b-1")),
            record_in_round("read_file", 1, Some("b-1")),
            record_in_round("str_replace", 2, None),
            record_in_round("bash", 3, None),
        ];

        let eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
            "fix the bug",
            &[],
            &records,
            0,
            false,
            0.26,
            EvaluationThresholds::default(),
            TurnEvaluationTelemetry {
                llm_rounds: Some(9),
                prompt_tokens: Some(136_947),
                first_round_prompt_tokens: Some(9_401),
                max_round_prompt_tokens: Some(20_954),
            },
        );

        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::HighCostLowYield { .. })),
            "progressive turn should not be over-penalized as high_cost_low_yield: {:?}",
            eval.signals
        );
        assert!(
            eval.success,
            "progressive high-cost turn should remain successful"
        );
    }

    #[test]
    fn tool_call_count_excludes_synthetic_placeholders() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // 3 real tool calls + 2 surgical removals = 5 records total,
        // but tool_call_count should be 3 (only real).
        let real = || ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: Some("content".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        let surgical = || ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: Some(0),
            args_preview: None,
            result_preview: Some("(removed)".into()),
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("glob".to_string()),
            ..Default::default()
        };
        let records = vec![real(), surgical(), real(), surgical(), real()];

        let event = build_turn_evaluation_journal_event(
            Some("sess-1"),
            Some(1),
            "test-model",
            "do something",
            &[],
            &records,
            0,
            false,
            0.5,
            &TurnEvaluation {
                success: true,
                quality: 0.8,
                confidence: 0.9,
                signals: vec![],
                thresholds: EvaluationThresholds::default(),
            },
        );

        // Extract tool_call_count from the metadata JSON.
        let metadata = event.metadata.as_ref().expect("metadata should be present");
        let tool_call_count = metadata["tool_call_count"].as_u64().unwrap();
        assert_eq!(
            tool_call_count, 3,
            "tool_call_count must exclude synthetic placeholders, got {}",
            tool_call_count
        );
    }

    #[test]
    fn is_synthetic_placeholder_via_flag() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // Current contract: surgically_removed flag set
        let flagged = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("bash".to_string()),
            ..Default::default()
        };
        assert!(flagged.is_synthetic_placeholder());

        // Sentinel name alone is not a supported synthetic marker.
        let unflagged_sentinel = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        assert!(!unflagged_sentinel.is_synthetic_placeholder());

        // Normal tool call: neither flag nor sentinel name
        let normal = ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        assert!(!normal.is_synthetic_placeholder());
    }

    // ─── Sequential read-churn (real-session-shaped fixtures) ───────────
    //
    // Real sessions inspected to calibrate the threshold:
    //   - 6566d6a8 turn 1: 10 consecutive single-tool read rounds → wasted
    //   - bbae8641 turn 3: 11 consecutive single-tool read rounds → wasted
    //   - 6da9cf8f turn 6: 16 consecutive single-tool read rounds → wasted
    //   - 03945541 turn 1: 6 single-tool rounds (locate→read chain) → healthy
    //   - 03945541 turn 2: max 4 single-tool rounds (mutate→verify) → healthy
    // Threshold of 8 cleanly separates these populations.

    fn record_in_round(name: &str, round: u32, batch: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            batch_id: batch.map(str::to_string),
            parallel: Some(batch.is_some()),
            round: Some(round),
            args_full: (name == "git").then(|| r#"{"action":"diff"}"#.to_string()),
            ..Default::default()
        }
    }

    fn record_in_round_with_args(
        name: &str,
        round: u32,
        batch: Option<&str>,
        args: serde_json::Value,
    ) -> ToolCallRecord {
        let mut record = record_in_round(name, round, batch);
        record.args_full = Some(args.to_string());
        record
    }

    #[test]
    fn exploration_family_churn_flags_repeated_git_action_diff_rounds() {
        let mut records = vec![record_in_round("git", 0, None)];
        for round in 1..4 {
            let batch = format!("b-{round}-0");
            for _ in 0..5 {
                records.push(record_in_round("git", round, Some(batch.as_str())));
            }
        }

        let eval = evaluate_tool_call_records("review local changes", &[], &records, 0, false, 0.3);
        let streak = eval.signals.iter().find_map(|signal| match signal {
            EvalSignal::ExplorationFamilyChurn { family, streak } => {
                Some((family.as_str(), *streak))
            }
            _ => None,
        });
        assert_eq!(
            streak,
            Some(("diff", 3)),
            "expected exploration-family diff streak, got {:?}",
            eval.signals
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::AllToolsHealthy)),
            "low-yield diff churn must not still look fully healthy: {:?}",
            eval.signals
        );

        let baseline =
            evaluate_tool_call_records("review local changes", &[], &records[..1], 0, false, 0.3);
        assert!(
            eval.quality < baseline.quality,
            "exploration-family churn quality={} should be below baseline={}",
            eval.quality,
            baseline.quality
        );
    }

    #[test]
    fn exploration_family_churn_flags_bash_and_structured_diff_rounds() {
        let records = vec![
            record_in_round_with_args(
                "bash",
                0,
                Some("b-0"),
                serde_json::json!({"command": "git diff -- src/"}),
            ),
            record_in_round_with_args(
                "git_diff",
                0,
                Some("b-0"),
                serde_json::json!({"path": "src", "ref": "HEAD"}),
            ),
            record_in_round_with_args(
                "bash",
                1,
                Some("b-1"),
                serde_json::json!({"command": "git --no-pager diff -- src/"}),
            ),
            record_in_round_with_args(
                "git_diff",
                1,
                Some("b-1"),
                serde_json::json!({"path": "src", "ref": "HEAD"}),
            ),
            record_in_round_with_args(
                "bash",
                2,
                Some("b-2"),
                serde_json::json!({"command": "git diff -- src/"}),
            ),
            record_in_round_with_args(
                "git_diff",
                2,
                Some("b-2"),
                serde_json::json!({"path": "src", "ref": "HEAD"}),
            ),
        ];

        let eval = evaluate_tool_call_records("review local changes", &[], &records, 0, false, 0.3);
        let streak = eval.signals.iter().find_map(|signal| match signal {
            EvalSignal::ExplorationFamilyChurn { family, streak } => {
                Some((family.as_str(), *streak))
            }
            _ => None,
        });
        assert_eq!(
            streak,
            Some(("diff", 3)),
            "expected mixed diff tools to count as diff-family churn, got {:?}",
            eval.signals
        );
    }

    #[test]
    fn exploration_family_churn_flags_repeated_search_rounds() {
        let records = vec![
            record_in_round("rg", 0, Some("b-0")),
            record_in_round("glob", 0, Some("b-0")),
            record_in_round("rg", 1, Some("b-1")),
            record_in_round("glob", 1, Some("b-1")),
            record_in_round("rg", 2, Some("b-2")),
            record_in_round("glob", 2, Some("b-2")),
        ];

        let eval =
            evaluate_tool_call_records("investigate auth flow", &[], &records, 0, false, 0.3);
        let streak = eval.signals.iter().find_map(|signal| match signal {
            EvalSignal::ExplorationFamilyChurn { family, streak } => {
                Some((family.as_str(), *streak))
            }
            _ => None,
        });
        assert_eq!(
            streak,
            Some(("search", 3)),
            "expected exploration-family search streak, got {:?}",
            eval.signals
        );
    }

    #[test]
    fn exploration_family_churn_does_not_flag_progressive_turn() {
        let records = vec![
            record_in_round("grep", 0, Some("b-0-0")),
            record_in_round("grep", 0, Some("b-0-0")),
            record_in_round("read_file", 1, Some("b-1-0")),
            record_in_round("read_file", 1, Some("b-1-0")),
            record_in_round("str_replace", 2, None),
            record_in_round("bash", 3, None),
        ];

        let eval = evaluate_tool_call_records("fix the bug", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|signal| matches!(signal, EvalSignal::ExplorationFamilyChurn { .. })),
            "progressive search→read→mutate→validate turn should stay silent: {:?}",
            eval.signals
        );
    }

    #[test]
    fn exploration_family_churn_signal_serializes_to_json() {
        let value = eval_signal_to_json(&EvalSignal::ExplorationFamilyChurn {
            family: "diff".to_string(),
            streak: 4,
        });
        assert_eq!(value["kind"], "exploration_family_churn");
        assert_eq!(value["family"], "diff");
        assert_eq!(value["streak"], 4);
        assert_eq!(
            value["threshold"],
            EXPLORATION_FAMILY_CHURN_THRESHOLD as i64
        );
    }

    #[test]
    fn high_cost_low_yield_signal_serializes_to_json() {
        let value = eval_signal_to_json(&EvalSignal::HighCostLowYield {
            tool_calls: 12,
            llm_rounds: Some(9),
        });
        assert_eq!(value["kind"], "high_cost_low_yield");
        assert_eq!(value["tool_calls"], 12);
        assert_eq!(value["llm_rounds"], 9);
    }
    // ─── Search fan-out detection ────────────────────────────────────────

    #[test]
    fn search_fanout_detects_many_search_calls() {
        let records = vec![
            record_with_args(
                "bash",
                0,
                r#"{"command":"grep -n 'canonicalize' src/a.rs | head -20"}"#,
            ),
            record_with_args(
                "bash",
                1,
                r#"{"command":"grep -n 'normalize_path' src/b.rs | head -20"}"#,
            ),
            record_with_args(
                "bash",
                2,
                r#"{"command":"find src -type f -name '*.rs' | xargs grep -l 'unique_path_variants' | head -20"}"#,
            ),
            record_with_args("grep", 3, r#"{"pattern":"canonicalize","path":"src/c.rs"}"#),
            record_with_args(
                "bash",
                4,
                r#"{"command":"cd /repo && rg 'file_state_key' src/ | head -20"}"#,
            ),
            record_with_args("rg", 5, r#"{"pattern":"project_root","path":"src/d.rs"}"#),
            record_with_args(
                "bash",
                6,
                r#"{"command":"git grep -n 'ToolExecutor' -- crates/astra-cli/src"}"#,
            ),
            record_with_args(
                "bash",
                7,
                r#"{"command":"grep -n 'shared_file_state' src/e.rs | head -20"}"#,
            ),
        ];
        let eval = evaluate_tool_call_records("investigate", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::SearchFanout(n) => Some(*n),
            _ => None,
        });
        assert_eq!(count, Some(8), "got {:?}", eval.signals);
    }

    #[test]
    fn search_fanout_below_threshold_is_silent() {
        let records = vec![
            record_with_args("bash", 0, r#"{"command":"grep -n 'a' src/a.rs"}"#),
            record_with_args("bash", 1, r#"{"command":"grep -n 'b' src/b.rs"}"#),
            record_with_args("bash", 2, r#"{"command":"grep -n 'c' src/c.rs"}"#),
            record_with_args("bash", 3, r#"{"command":"grep -n 'd' src/d.rs"}"#),
            record_with_args("bash", 4, r#"{"command":"grep -n 'e' src/e.rs"}"#),
            record_with_args("grep", 5, r#"{"pattern":"f","path":"src/f.rs"}"#),
            record_with_args("rg", 6, r#"{"pattern":"g","path":"src/g.rs"}"#),
        ];
        let eval = evaluate_tool_call_records("investigate", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::SearchFanout(_))),
            "7 search calls is below threshold 8; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn search_fanout_ignores_non_search_calls() {
        let records = vec![
            record_with_args("bash", 0, r#"{"command":"cat src/a.rs"}"#),
            record_with_args("bash", 1, r#"{"command":"sed -n '1,20p' src/b.rs"}"#),
            record_with_args(
                "read_file",
                2,
                r#"{"path":"src/c.rs","start_line":1,"end_line":20}"#,
            ),
            record_with_args("read_file", 3, r#"{"path":"src/d.rs"}"#),
            record_with_args("git", 4, r#"{"action":"show","revision":"HEAD"}"#),
        ];
        let eval = evaluate_tool_call_records("investigate", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::SearchFanout(_))),
            "non-search calls must not trigger SearchFanout; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn search_fanout_custom_threshold_flows_into_json() {
        let value = eval_signal_to_json_with_thresholds(
            &EvalSignal::SearchFanout(5),
            EvaluationThresholds {
                search_fanout: 4,
                ..Default::default()
            },
        );
        assert_eq!(value["kind"], "search_fanout");
        assert_eq!(value["count"], 5);
        assert_eq!(value["threshold"], 4);
        assert!(value["message"].as_str().unwrap().contains("grep/rg/find"));
    }

    #[test]
    fn redundant_validation_retries_detects_same_prefix_with_output_shaping_variants() {
        let records = vec![
            record_with_args("bash", 0, r#"{"command":"cd tmp && cargo check 2>&1"}"#),
            record_with_args(
                "bash",
                1,
                r#"{"command":"cd tmp && cargo check 2>&1 | head -30"}"#,
            ),
            record_with_args(
                "bash",
                2,
                r#"{"command":"cd tmp && cargo check 2>&1 | tail -20"}"#,
            ),
        ];
        let eval = evaluate_tool_call_records("fix build", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantValidationRetries(n) => Some(*n),
            _ => None,
        });
        assert_eq!(count, Some(2), "got {:?}", eval.signals);
    }

    #[test]
    fn redundant_validation_retries_below_threshold_is_silent() {
        let records = vec![
            record_with_args("bash", 0, r#"{"command":"cd tmp && cargo check 2>&1"}"#),
            record_with_args(
                "bash",
                1,
                r#"{"command":"cd tmp && cargo check 2>&1 | head -30"}"#,
            ),
        ];
        let eval = evaluate_tool_call_records("fix build", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RedundantValidationRetries(_))),
            "single retry should stay below threshold; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_validation_retries_reset_after_mutation() {
        let records = vec![
            record_with_args("bash", 0, r#"{"command":"cd tmp && cargo check 2>&1"}"#),
            record_with_args(
                "bash",
                1,
                r#"{"command":"cd tmp && cargo check 2>&1 | head -30"}"#,
            ),
            record_with_args(
                "str_replace",
                2,
                r#"{"path":"tmp/src/main.rs","old_str":"a","new_str":"b"}"#,
            ),
            record_with_args(
                "bash",
                3,
                r#"{"command":"cd tmp && cargo check 2>&1 | tail -20"}"#,
            ),
            record_with_args(
                "bash",
                4,
                r#"{"command":"cd tmp && cargo check 2>&1 | grep -A3 '^error:'"}"#,
            ),
        ];
        let eval = evaluate_tool_call_records("fix build", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RedundantValidationRetries(_))),
            "mutation should reset retry history; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_validation_retries_custom_threshold_flows_into_json() {
        let value = eval_signal_to_json_with_thresholds(
            &EvalSignal::RedundantValidationRetries(3),
            EvaluationThresholds {
                redundant_validation_retries: 2,
                ..Default::default()
            },
        );
        assert_eq!(value["kind"], "redundant_validation_retries");
        assert_eq!(value["count"], 3);
        assert_eq!(value["threshold"], 2);
    }

    // ─── Redundant overlapping reads detection ──────────────────────────
    //
    // Calibration fixtures (real session shapes; see commit message):
    //   - 8ba9d165 turn 2: 4×`sed -n '159,200p' execution_phase.rs`,
    //     4×`sed -n '840,870p' …`, etc. — count >= 17.
    //   - eafda07e turn 2: 19 redundant reads, 18 rounds, 259k tokens.
    // Threshold 3 catches all known waste fixtures while leaving healthy
    // turns silent.

    fn record_with_args(name: &str, round: u32, args_full: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: Some(args_full.chars().take(80).collect()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            batch_id: None,
            parallel: Some(false),
            round: Some(round),
            args_full: Some(args_full.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn redundant_reads_detects_repeated_sed_ranges_in_same_file() {
        // Mirrors 8ba9d165 turn 2: same line range read 4 times across rounds,
        // no edits in between — pure waste.
        let records = vec![
            record_with_args(
                "bash",
                0,
                "sed -n '159,200p' crates/runtime/src/turn/agentic_loop_execution_phase.rs",
            ),
            record_with_args(
                "bash",
                3,
                "sed -n '159,200p' crates/runtime/src/turn/agentic_loop_execution_phase.rs",
            ),
            record_with_args(
                "bash",
                6,
                "sed -n '159,200p' crates/runtime/src/turn/agentic_loop_execution_phase.rs",
            ),
            record_with_args(
                "bash",
                9,
                "sed -n '159,200p' crates/runtime/src/turn/agentic_loop_execution_phase.rs",
            ),
        ];
        let eval = evaluate_tool_call_records("修复优化", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantOverlappingReads(n) => Some(*n),
            _ => None,
        });
        assert_eq!(
            count,
            Some(3), // 3 reads after the first overlap = 3 redundant events
            "expected RedundantOverlappingReads(3), got {:?}",
            eval.signals
        );
    }

    #[test]
    fn extending_overlapping_ranges_are_new_evidence() {
        // Every request extends beyond the previously delivered range. Partial
        // overlap alone is not redundancy because the model receives unseen
        // content on each call.
        let records = vec![
            record_with_args("bash", 0, "sed -n '100,150p' src/foo.rs"),
            record_with_args("bash", 1, "sed -n '120,180p' src/foo.rs"),
            record_with_args("bash", 2, "sed -n '140,200p' src/foo.rs"),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantOverlappingReads(n) => Some(*n),
            _ => None,
        });
        assert_eq!(
            count, None,
            "extending ranges must not be classified as redundant"
        );
    }

    #[test]
    fn redundant_reads_excludes_mutate_then_verify_pattern() {
        // Read → edit → read of same range is healthy verification, not waste.
        // We need 4 reads to potentially cross threshold; with an edit
        // between the first 2 and last 2, the count must reset and stay <3.
        let records = vec![
            record_with_args("bash", 0, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 1, "sed -n '10,50p' src/foo.rs"),
            // Edit invalidates per-file history.
            record_with_args(
                "str_replace",
                2,
                r#"{"path":"src/foo.rs","old_str":"x","new_str":"y"}"#,
            ),
            record_with_args("bash", 3, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 4, "sed -n '10,50p' src/foo.rs"),
        ];
        let eval = evaluate_tool_call_records("fix bug", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantOverlappingReads(n) => Some(*n),
            _ => None,
        });
        // Pre-edit: 1 redundant. Post-edit: 1 redundant. Total = 2 < threshold(3).
        assert_eq!(
            count, None,
            "mutate→verify pattern must not flag — got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_reads_does_not_flag_distinct_files() {
        // Three reads of different files = no overlap, no signal.
        let records = vec![
            record_with_args("bash", 0, "sed -n '1,50p' src/a.rs"),
            record_with_args("bash", 1, "sed -n '1,50p' src/b.rs"),
            record_with_args("bash", 2, "sed -n '1,50p' src/c.rs"),
            record_with_args("bash", 3, "sed -n '1,50p' src/d.rs"),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RedundantOverlappingReads(_))),
            "distinct files should not trigger redundancy; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_reads_signal_requires_full_prior_coverage() {
        // Prior ranges can jointly cover a later request. This is redundant
        // even when no single prior request contains the whole range.
        let records = vec![
            record_with_args(
                "read_file",
                0,
                r#"{"path":"src/foo.rs","start_line":10,"end_line":50}"#,
            ),
            record_with_args(
                "read_file",
                1,
                r#"{"path":"src/foo.rs","start_line":51,"end_line":80}"#,
            ),
            record_with_args(
                "read_file",
                2,
                r#"{"path":"src/foo.rs","start_line":20,"end_line":70}"#,
            ),
            record_with_args(
                "read_file",
                3,
                r#"{"path":"src/foo.rs","start_line":25,"end_line":65}"#,
            ),
            record_with_args(
                "read_file",
                4,
                r#"{"path":"src/foo.rs","start_line":30,"end_line":60}"#,
            ),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantOverlappingReads(n) => Some(*n),
            _ => None,
        });
        assert_eq!(count, Some(3), "got {:?}", eval.signals);
    }

    #[test]
    fn redundant_reads_recognizes_read_file_tool() {
        let records = vec![
            record_with_args("read_file", 0, r#"{"path":"src/main.rs"}"#),
            record_with_args("read_file", 1, r#"{"path":"src/main.rs"}"#),
            record_with_args("read_file", 2, r#"{"path":"src/main.rs"}"#),
            record_with_args("read_file", 3, r#"{"path":"src/main.rs"}"#),
        ];
        let count = count_redundant_overlapping_reads(&records);
        assert_eq!(
            count, 3,
            "read_file re-reads of the same file should count as redundant"
        );
    }

    #[test]
    fn active_redundant_reads_resolve_after_authoritative_mutation() {
        let mut records = vec![
            record_with_args("read_file", 0, r#"{"path":"src/main.rs"}"#),
            record_with_args("read_file", 1, r#"{"path":"src/main.rs"}"#),
            record_with_args("read_file", 2, r#"{"path":"src/main.rs"}"#),
            record_with_args("read_file", 3, r#"{"path":"src/main.rs"}"#),
        ];
        assert_eq!(count_active_redundant_overlapping_reads(&records), 3);

        let mut failed_mutation = record_with_args("str_replace", 4, r#"{"path":"src/main.rs"}"#);
        failed_mutation.ok = false;
        records.push(failed_mutation);
        assert_eq!(
            count_active_redundant_overlapping_reads(&records),
            3,
            "a failed mutation cannot invalidate successful read evidence"
        );

        records.push(record_with_args(
            "str_replace",
            5,
            r#"{"path":"src/main.rs"}"#,
        ));
        assert_eq!(
            count_redundant_overlapping_reads(&records),
            3,
            "terminal audit retains the historical inefficiency"
        );
        assert_eq!(
            count_active_redundant_overlapping_reads(&records),
            0,
            "online feedback resolves after the relevant state changed"
        );

        let mut failed_reads = vec![
            record_with_args("read_file", 0, r#"{"path":"src/failed.rs"}"#),
            record_with_args("read_file", 1, r#"{"path":"src/failed.rs"}"#),
            record_with_args("read_file", 2, r#"{"path":"src/failed.rs"}"#),
            record_with_args("read_file", 3, r#"{"path":"src/failed.rs"}"#),
        ];
        failed_reads.iter_mut().for_each(|record| record.ok = false);
        assert_eq!(
            count_active_redundant_overlapping_reads(&failed_reads),
            0,
            "failed reads never become online behavioral evidence"
        );
    }

    #[test]
    fn extending_read_file_ranges_are_not_redundant() {
        let records = vec![
            record_with_args(
                "read_file",
                0,
                r#"{"path":"src/lib.rs","start_line":1,"end_line":50}"#,
            ),
            record_with_args(
                "read_file",
                1,
                r#"{"path":"src/lib.rs","start_line":20,"end_line":80}"#,
            ),
            record_with_args(
                "read_file",
                2,
                r#"{"path":"src/lib.rs","start_line":40,"end_line":100}"#,
            ),
        ];
        let count = count_redundant_overlapping_reads(&records);
        assert_eq!(count, 0, "each read contributes previously unseen lines");
    }

    #[test]
    fn unbounded_read_does_not_claim_full_file_delivery() {
        let records = vec![
            record_with_args("read_file", 0, r#"{"path":"src/lib.rs"}"#),
            record_with_args(
                "read_file",
                1,
                r#"{"path":"src/lib.rs","start_line":200,"end_line":260}"#,
            ),
        ];
        assert_eq!(
            count_redundant_overlapping_reads(&records),
            0,
            "an output-bounded whole-file request cannot prove later lines were delivered"
        );
    }

    #[test]
    fn redundant_reads_signal_serializes_to_json() {
        let v = eval_signal_to_json(&EvalSignal::RedundantOverlappingReads(5));
        assert_eq!(v["kind"], "redundant_overlapping_reads");
        assert_eq!(v["count"], 5);
        assert_eq!(v["threshold"], REDUNDANT_OVERLAPPING_READS_THRESHOLD as i64);
        assert!(v["message"].as_str().unwrap().contains("redundant"));
    }

    #[test]
    fn redundant_reads_custom_threshold_flows_into_journal_json() {
        let records = vec![
            record_with_args("bash", 0, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 1, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 2, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 3, "sed -n '10,50p' src/foo.rs"),
        ];
        let eval = evaluate_tool_call_records_with_thresholds(
            "fix bug",
            &[],
            &records,
            0,
            false,
            0.3,
            EvaluationThresholds {
                redundant_overlapping_reads: 3,
                ..Default::default()
            },
        );
        let event = build_turn_evaluation_journal_event(
            Some("sess-1"),
            Some(1),
            "cli_repl",
            "fix bug",
            &[],
            &records,
            0,
            false,
            0.3,
            &eval,
        );
        let metadata = event.metadata.expect("turn evaluation metadata");
        let redundant = metadata["signals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|signal| signal["kind"] == "redundant_overlapping_reads")
            .expect("redundant signal");
        assert_eq!(redundant["count"], 3);
        assert_eq!(redundant["threshold"], 3);
    }

    #[test]
    fn redundant_reads_below_threshold_is_silent() {
        // 2 redundant reads (threshold=3) must not emit signal.
        let records = vec![
            record_with_args("bash", 0, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 1, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 2, "sed -n '10,50p' src/foo.rs"),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RedundantOverlappingReads(_))),
            "2 redundant reads must be silent (threshold=3); got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_reads_grep_and_other_search_tools_not_counted() {
        // grep/ls/glob are search tools, not "read region of file" — they
        // must not contribute to redundancy even if same path repeats.
        let records = vec![
            record_with_args("bash", 0, "grep -n 'foo' src/x.rs"),
            record_with_args("bash", 1, "grep -n 'bar' src/x.rs"),
            record_with_args("bash", 2, "grep -n 'baz' src/x.rs"),
            record_with_args("bash", 3, "grep -n 'qux' src/x.rs"),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RedundantOverlappingReads(_))),
            "grep calls must not be classified as reads; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn compound_shell_receipt_must_follow_the_last_possible_mutation() {
        assert!(bash_command_has_post_mutation_validation(
            "rm -rf build && git push origin main && cat dist/index.html && curl -sk https://localhost/"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "set -e; cargo test -p example; test -f target/debug/example"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "sed -i 's/np.int/np.intp/' source.py && python setup.py build_ext --inplace 2>&1 | tail -20"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "python3 -m build --wheel"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "python3 -m pytest 2>&1"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "python3 -m pytest 1>&2"
        ));
        // A validator followed by an alternate/fallback command is not a
        // status-determining receipt: the shell can report success even when
        // the validator failed.
        assert!(!bash_command_has_post_mutation_validation(
            "cargo test || true"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "cargo test; true"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "cargo test && true"
        ));
        assert!(bash_command_post_mutation_validation_prefix("cargo test || true").is_none());
        assert!(bash_command_post_mutation_validation_prefix("cargo test; true").is_none());
        assert!(bash_command_post_mutation_validation_prefix("cargo test && true").is_some());
        assert!(bash_command_has_post_mutation_validation("cargo test;"));
        assert!(bash_command_has_post_mutation_validation("cargo test\n"));
        assert!(bash_command_has_post_mutation_validation(
            "cargo test # comment\n"
        ));
        assert!(bash_command_post_mutation_validation_prefix("cargo test;").is_some());
        assert!(bash_command_post_mutation_validation_prefix("cargo test\n").is_some());
        assert!(bash_command_post_mutation_validation_prefix("cargo test # comment\n").is_some());
        assert!(!bash_command_has_post_mutation_validation(
            "python3 -m pytest 1>&1"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "python3 -m pytest 2>&2"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "python3 -m pytest 3>&1"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "python3 -m pytest &"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "cat dist/index.html && rm -rf build"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "rm -rf build && echo done"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "curl -X POST https://localhost/deploy"
        ));
        assert_eq!(
            bash_command_post_mutation_validation_prefix(
                "sed -i 's/a/b/' src/lib.rs && cargo test"
            ),
            Some("cargo test".into())
        );
        assert_eq!(
            bash_command_post_mutation_validation_prefix(
                "cargo test && sed -i 's/a/b/' src/lib.rs && test -e src/lib.rs"
            ),
            None
        );
        assert!(bash_command_post_mutation_validation_prefix("npx tsc --noEmit").is_some());
        assert!(bash_command_post_mutation_validation_prefix("npm run build").is_some());
        assert!(
            bash_command_post_mutation_validation_prefix("python setup.py build_ext --inplace")
                .is_some()
        );
        for command in [
            "cargo test --help",
            "cargo test --version",
            "cargo test '--help'",
            "cargo test \"$MODE\"",
            "pytest --help",
            "pytest \"$MODE\"",
            "python3 -m pytest -h",
            "npx tsc --noEmit --help",
        ] {
            assert!(
                !bash_command_has_post_mutation_validation(command),
                "metadata mode is not validation evidence: {command}"
            );
            assert!(
                bash_command_post_mutation_validation_prefix(command).is_none(),
                "metadata mode is not a strict validation receipt: {command}"
            );
        }
        for command in ["cargo test -v", "go test -v", "pytest -v"] {
            assert!(
                bash_command_post_mutation_validation_prefix(command).is_some(),
                "verbose validation must remain a receipt: {command}"
            );
        }
        for command in [
            "touch /workspace/out | cargo test",
            "cargo test | touch /workspace/out",
            "time cp /workspace/source /workspace/out | cargo test",
            "time cat /workspace/source > /workspace/out | cargo test",
            "cd /workspace > /workspace/out | cargo test",
            "set -e > /workspace/out | cargo test",
            "cat /workspace/source > /workspace/output",
        ] {
            assert!(
                !bash_command_has_post_mutation_validation(command),
                "a concurrent writer cannot be ordered around a validator: {command}"
            );
            assert!(
                bash_command_post_mutation_validation_prefix(command).is_none(),
                "a concurrent writer cannot produce a strict receipt: {command}"
            );
        }
        // A `>` inside a quoted argument is data, not shell redirection. The
        // same raw-syntax predicate is consumed by runtime lifecycle checks.
        assert!(bash_command_has_post_mutation_validation(
            "grep '>' /workspace/file"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "cat /workspace/source > /workspace/output && cat /workspace/output"
        ));
        assert_eq!(
            split_static_shell_words("cp src '/tmp/volatile file'"),
            Some(vec!["cp".into(), "src".into(), "/tmp/volatile file".into()])
        );
        assert_eq!(
            split_static_shell_words("printf x > '/workspace/out file'"),
            Some(vec![
                "printf".into(),
                "x".into(),
                ">".into(),
                "/workspace/out file".into()
            ])
        );
        assert_eq!(
            split_static_shell_words("sed -i 's/$/x/' '/workspace/out file'"),
            Some(vec![
                "sed".into(),
                "-i".into(),
                "s/$/x/".into(),
                "/workspace/out file".into()
            ])
        );
        assert!(split_static_shell_words("cp $DEST source").is_none());
        assert!(split_static_shell_words("cp src 'unterminated").is_none());
        assert!(split_static_shell_words("cp src | tee out").is_none());
    }

    #[test]
    fn nested_read_receipts_inside_assignments_and_output_are_evidence() {
        assert!(bash_command_has_post_mutation_validation(
            r#"rm -rf work; deploy-command; value=$(curl -fsS https://localhost/); echo "state=$value""#
        ));
        assert!(bash_command_has_post_mutation_validation(
            r#"git push origin main; echo "state=$(curl -fsS https://localhost/ | tr -d '\n')""#
        ));
        assert!(bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(test -f result.json && echo PASS || echo FAIL)""#
        ));
        assert!(bash_command_has_post_mutation_validation(
            "mutate-command\nvalue=$(curl -fsS https://service/ | tr -d '\\n')\necho \"status=$([ \\\"$value\\\" = expected ] && echo PASS || echo FAIL)\"\n"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "mutate-command\ncat result.json # ; $(unknown_probe) `unknown_probe` &\n"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "mutate-command\necho \"state=#not-a-comment\"\ncat result.json # preserve receipt\n"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "mutate-command\nvalue=$(curl -fsS https://service/#fragment)\n"
        ));
    }

    #[test]
    fn nested_unknown_or_post_receipt_writes_do_not_fake_validation() {
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(unknown_probe)""#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(cat result.json)" > report.txt"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; value=$(cat result.json; unknown_probe)"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(curl -X POST https://localhost/deploy)""#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(cat result.json)" | tee changed.txt"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo '$(cat result.json)'"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(cat result.json""#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; value=$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(cat result.json)"))))))))))))))))")"#
        ));
        let oversized = format!(
            "cat result.json; {}unknown-write",
            "x".repeat(MAX_SHELL_LEX_BYTES)
        );
        assert!(!bash_command_has_post_mutation_validation(&oversized));
        assert!(!bash_command_has_post_mutation_validation(
            r#"cat result.json; echo "unterminated"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"cat result.json; echo "$(cat result.json"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"cat result.json; echo `unknown_probe`"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "cat result.json; echo \"$(cat result.json)\" & unknown-write"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "cat result.json <<EOF\nunknown-write\nEOF"
        ));
    }
}
