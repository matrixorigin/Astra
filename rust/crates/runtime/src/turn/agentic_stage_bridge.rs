//! Bridge between the runtime's `AgenticLoopState` and the `astra-pipeline`
//! rule-based stages (`ReflectStage::diagnose`, `EvaluateStage::categorize_progress`).
//!
//! The full `ExecutionEngine` operates on its own [`TurnState`] struct, which
//! duplicates the runtime's agentic-loop state. Rather than rewriting the
//! agentic loop to run on `TurnState`, this bridge builds a *minimal* synthetic
//! `TurnState` from the runtime's stall / tool records and invokes the pure
//! rule-based helpers. Results are surfaced to the LLM auto-reflection prompt
//! via `AgenticLoopState::recent_tactical_actions`, giving the model a
//! structured diagnosis to reason against.
//!
//! This is intentionally narrow — it does not drive the loop's state machine.
//! It is a *diagnostic augmentation* layer that complements (not replaces) the
//! runtime's own stall/auto-reflection machinery.
use std::collections::HashSet;

use astra_pipeline::stages::evaluate::{ProgressCategory, categorize_progress};
use astra_pipeline::stages::reflect::{FailureCategory, compute_strategy_delta, diagnose};
use astra_pipeline::state::{StrategyDelta, TurnState};

use super::agentic_loop_host::AgenticLoopState;

/// Structured diagnosis produced by the pipeline stages when run against a
/// runtime-derived snapshot.
#[derive(Debug, Clone)]
pub struct PipelineDiagnosis {
    pub failure_category: FailureCategory,
    pub progress_category: ProgressCategory,
    pub what_happened: String,
    pub what_to_try: String,
    pub strategy: StrategyDelta,
}

impl PipelineDiagnosis {
    /// A short single-line tactical-action label suitable for injecting into
    /// the auto-reflection prompt context.
    pub fn tactical_action_label(&self) -> String {
        format!(
            "pipeline-diagnose[{:?}/{:?}]: {} → {}",
            self.failure_category, self.progress_category, self.what_happened, self.what_to_try,
        )
    }
}

/// Build a minimal pipeline [`TurnState`] snapshot from raw runtime signals.
fn snapshot_turn_state_from_signals(
    tool_records: &[astra_services::session_journal::ToolCallRecord],
    turn_tool_names: &[HashSet<String>],
) -> TurnState {
    let mut snapshot = TurnState::new("<bridge>", Vec::new(), 1, 1, 1);

    for record in tool_records {
        if !record.ok {
            let err = record.error.clone().unwrap_or_else(|| "error".to_string());
            snapshot
                .tool_failures
                .entry(record.name.clone())
                .or_default()
                .push(err);
        }
    }

    snapshot.round_tool_signatures = turn_tool_names
        .iter()
        .map(|set| set.iter().cloned().collect::<HashSet<String>>())
        .collect();

    let total_records = tool_records.len();
    let failed_records = tool_records.iter().filter(|r| !r.ok).count();
    snapshot.total_tool_calls = total_records as u32;

    if total_records > 0 {
        let success_ratio = 1.0 - (failed_records as f64 / total_records.max(1) as f64);
        snapshot.progress.record(success_ratio);
    }

    snapshot
}

/// Run the pipeline rule-based diagnosis directly from raw runtime signals.
pub fn diagnose_from_signals(
    tool_records: &[astra_services::session_journal::ToolCallRecord],
    turn_tool_names: &[HashSet<String>],
) -> PipelineDiagnosis {
    let snapshot = snapshot_turn_state_from_signals(tool_records, turn_tool_names);
    let (failure_category, what_happened, what_to_try) = diagnose(&snapshot);
    let strategy = compute_strategy_delta(&snapshot, failure_category);
    let progress_category = categorize_progress(&snapshot.progress);
    PipelineDiagnosis {
        failure_category,
        progress_category,
        what_happened,
        what_to_try,
        strategy,
    }
}

/// Run the pipeline rule-based diagnosis against a runtime state snapshot.
pub fn diagnose_from_loop_state(state: &AgenticLoopState) -> PipelineDiagnosis {
    diagnose_from_signals(&state.stall.tool_call_records, &state.stall.turn_tool_names)
}

/// Summary of what the strategy-delta application actually changed in the
/// runtime state — used for logging / tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrategyApplication {
    /// Tools newly inserted into `state.restricted_tools`.
    pub newly_blocked: Vec<String>,
    /// Tools already present (no-op inserts).
    pub already_blocked: Vec<String>,
    /// Whether `widen_selection` was requested by the strategy.
    pub widen_requested: bool,
}

impl StrategyApplication {
    pub fn is_noop(&self) -> bool {
        self.newly_blocked.is_empty() && !self.widen_requested
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.newly_blocked.is_empty() {
            parts.push(format!("blocked={:?}", self.newly_blocked));
        }
        if !self.already_blocked.is_empty() {
            parts.push(format!("already_blocked={:?}", self.already_blocked));
        }
        if self.widen_requested {
            parts.push("widen_selection=true".to_string());
        }
        if parts.is_empty() {
            "noop".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Apply a pipeline [`StrategyDelta`] to the runtime [`AgenticLoopState`]:
/// - `block_tools` → inserted into `state.restricted_tools` (the runtime's
///   canonical block-list consulted by tool selection and execution policy).
/// - `widen_selection` → reported in the returned summary for observability;
///   the tool-selection path reads `restricted_tools` directly, so widening
///   here effectively means "nothing additional is blocked beyond the newly
///   added names".
/// - `add_tools` and `inject_context` are intentionally not applied here:
///   the runtime has no positive-allowlist channel equivalent to
///   `TurnState::add_tools`, and injection is already handled textually via
///   the tactical-action label emitted by [`PipelineDiagnosis`].
pub fn apply_strategy_delta(
    state: &mut AgenticLoopState,
    strategy: &StrategyDelta,
) -> StrategyApplication {
    let mut app = StrategyApplication {
        widen_requested: strategy.widen_selection,
        ..Default::default()
    };
    for tool in &strategy.block_tools {
        if state.restricted_tools.insert(tool.clone()) {
            app.newly_blocked.push(tool.clone());
        } else {
            app.already_blocked.push(tool.clone());
        }
    }
    app.newly_blocked.sort();
    app.already_blocked.sort();
    app
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::ToolCallRecord;
    use std::collections::HashSet;

    fn record(name: &str, ok: bool, err: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 1,
            error: err.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn bridge_detects_tool_failures_and_recommends_block() {
        let records = vec![
            record("flaky_http", false, Some("500")),
            record("flaky_http", false, Some("500")),
            record("flaky_http", false, Some("timeout")),
        ];
        let diag = diagnose_from_signals(&records, &[]);
        assert_eq!(diag.failure_category, FailureCategory::ToolFailures);
        assert!(
            diag.strategy.block_tools.iter().any(|t| t == "flaky_http"),
            "expected flaky_http in block_tools, got {:?}",
            diag.strategy.block_tools,
        );
        assert!(diag.strategy.widen_selection);
        let label = diag.tactical_action_label();
        assert!(label.contains("pipeline-diagnose"), "label: {}", label);
        assert!(label.contains("flaky_http"));
    }

    #[test]
    fn bridge_detects_stall_via_repeated_tool_signatures() {
        let sig: HashSet<String> = ["grep".to_string(), "read".to_string()]
            .into_iter()
            .collect();
        let sigs = vec![sig.clone(), sig.clone(), sig.clone()];
        let records = vec![record("grep", true, None), record("read", true, None)];
        let diag = diagnose_from_signals(&records, &sigs);
        assert_eq!(diag.failure_category, FailureCategory::Stall);
        assert!(diag.strategy.widen_selection);
        assert!(diag.strategy.inject_context.is_some());
    }

    #[test]
    fn bridge_clean_state_falls_back_to_general() {
        let diag = diagnose_from_signals(&[], &[]);
        assert_eq!(diag.failure_category, FailureCategory::General);
    }

    #[test]
    fn apply_strategy_delta_blocks_new_tools() {
        let records = vec![
            record("flaky_http", false, Some("500")),
            record("flaky_http", false, Some("500")),
            record("flaky_http", false, Some("timeout")),
        ];
        let diag = diagnose_from_signals(&records, &[]);
        // Diagnosis should recommend blocking the failing tool; the concrete
        // end-to-end application against AgenticLoopState is exercised in the
        // `auto_reflection_injects_pipeline_diagnosis_into_prompt` e2e test.
        assert!(!diag.strategy.block_tools.is_empty());
        assert!(
            diag.strategy
                .block_tools
                .contains(&"flaky_http".to_string())
        );
    }
}
