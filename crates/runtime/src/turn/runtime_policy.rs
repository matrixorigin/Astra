//! Runtime policy engine: translates pure `JournalFacts` from the
//! `ObservationJournal` into structured `RuntimePolicyEvidence`.
//!
//! # Separation of concerns
//!
//! | Layer | Location | Responsibility |
//! |-------|----------|----------------|
//! | `JournalFacts` | `astra_core::observation_journal` | Pure factual snapshot — counts, streaks, no judgments |
//! | `ObservationJournal` | `astra_core::observation_journal` | Data collection: record_turn, extract_facts, trends |
//! | **`RuntimePolicy`** | `astra_runtime::turn::runtime_policy` | Evidence engine: facts → advisories |
//!
//! The policy **never** inspects internal state beyond the `JournalFacts`
//! snapshot. It is the runtime's responsibility — not the core data layer's —
//! to surface advisory evidence based on observed facts.

use astra_core::observation_journal::{
    BudgetSnapshot, JournalFacts, PerformanceSnapshot, StallSnapshot, StreakSnapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};

use astra_services::session_journal::{ToolCallDisposition, ToolCallRecord};
use astra_turn_core::context_feedback::{
    RuntimePolicyFeedbackEntry, RuntimePolicyFeedbackSet, RuntimePolicyRecommendation,
    RuntimePolicySignal, RuntimePolicyStage, RuntimePolicySubject,
};

pub fn configured_evaluation_thresholds() -> astra_turn_core::evaluation::EvaluationThresholds {
    let policy = astra_config::RuntimeConfig::load().tool_policy;
    astra_turn_core::evaluation::EvaluationThresholds {
        redundant_overlapping_reads: policy.effective_redundant_reads_eval_threshold() as usize,
        search_fanout: policy.effective_search_fanout_eval_threshold() as usize,
        redundant_validation_retries: policy.effective_redundant_validation_retries_eval_threshold()
            as usize,
        llm_round_churn: astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD,
        exploration_family_churn: astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
    }
}

// ─── Framework Actions ────────────────────────────────────────────────────────

/// Urgency level for context-pressure guidance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ContextPressureUrgency {
    /// Elevated token pressure: conserve context, but continue normally.
    Normal,
    /// Critical token pressure: strongly prefer synthesis or a narrow next action.
    Aggressive,
}

impl Default for ContextPressureUrgency {
    fn default() -> Self {
        Self::Normal
    }
}

impl std::fmt::Display for ContextPressureUrgency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(f, "normal"),
            Self::Aggressive => write!(f, "aggressive"),
        }
    }
}

/// Structured evidence emitted by the runtime policy engine.
///
/// None of these variants authorizes retries, phase changes, budget mutation,
/// tool restrictions, or termination. The execution loop only serializes them
/// into the typed advisory lane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimePolicyEvidence {
    /// Evidence supporting a possible budget expansion.
    BudgetExpansionSuggested {
        factor: f64,
        /// Absolute ceiling — never exceed this regardless of factor.
        max_ceiling: u32,
    },
    /// Observed high token pressure.
    ///
    /// This is intentionally not a compaction event. Real compaction events are
    /// emitted only by the lifecycle/retry compression pipelines after they
    /// actually free tokens.
    ContextPressureObserved { urgency: ContextPressureUrgency },
    /// General advisory evidence for the next round.
    Advisory { message: String },
    /// No advisory evidence was produced.
    NoAdvisory,
}

// ─── Context Pressure Policy ─────────────────────────────────────────────────

/// Policy for agent-visible guidance under token pressure.
///
/// When `token_pressure` (from `JournalFacts`) exceeds the threshold, the
/// policy engine requests `RuntimePolicyEvidence::ContextPressureObserved` with the
/// configured urgency level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPressurePolicy {
    /// Token pressure threshold (0.0–1.0) above which Normal guidance fires.
    pub pressure_threshold: f64,
    /// Token pressure threshold (0.0–1.0) above which Aggressive guidance fires.
    pub aggressive_pressure_threshold: f64,
}

impl Default for ContextPressurePolicy {
    fn default() -> Self {
        Self {
            pressure_threshold: 0.70,
            aggressive_pressure_threshold: 0.90,
        }
    }
}

// ─── Circuit Breaker Policy ──────────────────────────────────────────────────

/// Policy for surfacing circuit-breaker risk based on error rate and read-only
/// streaks.
///
/// Read-only investigation is often a valid phase of large tasks. These
/// thresholds therefore drive diagnostic signals to the model instead of
/// reducing the turn budget. Hard stops remain the job of explicit guard and
/// tool-execution failures, not passive exploration alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitPolicy {
    /// Max consecutive zero-outcome rounds before a diagnostic signal.
    pub max_consecutive_errors: u32,
    /// Max consecutive read-only rounds before a diagnostic signal.
    pub max_consecutive_reads: u32,
    /// Error rate (0.0–1.0) above which a diagnostic signal is emitted.
    pub error_rate_threshold: f64,
}

impl Default for CircuitPolicy {
    fn default() -> Self {
        Self {
            max_consecutive_errors: 5,
            max_consecutive_reads: 8,
            error_rate_threshold: 0.30,
        }
    }
}

// ─── Runtime Policy ──────────────────────────────────────────────────────��───

/// Runtime policy that uses purely factual thresholds — consecutive outcomes
/// or non-outcomes — rather than scored "progress" heuristics.
///
/// Every parameter is user-configurable; nothing is hardcoded.
///
/// Composes four sub-policies:
/// - `RuntimePolicy` core: budget expansion, signal injection for streaks
/// - `ContextPressurePolicy`: guidance under token pressure
/// - `CircuitPolicy`: diagnostic guidance under errors / read-only streaks
///
/// This lives in the runtime crate (not core) because it interprets facts into
/// advisory evidence.
/// Core only collects data (`ObservationJournal`) and exposes pure facts
/// (`JournalFacts`). The runtime decides what to do with those facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePolicy {
    /// Expand budget after this many consecutive rounds with observable outcome.
    pub expand_after_consecutive_outcomes: u32,
    /// Multiply current max rounds by this factor on expansion.
    pub expand_factor: f64,
    /// Absolute ceiling: budget never exceeds this regardless of expansions.
    pub max_ceiling: u32,
    /// Transition to reflection after this many consecutive rounds with zero outcome.
    pub reflect_after_consecutive_zero: u32,
    /// Context-pressure sub-policy.
    #[serde(default)]
    pub context_pressure: ContextPressurePolicy,
    /// Circuit breaker sub-policy.
    #[serde(default)]
    pub circuit: CircuitPolicy,
    /// Append a marker when provider output was truncated by its token limit.
    #[serde(default = "default_mark_truncated_text")]
    pub mark_truncated_text: bool,
}

const fn default_mark_truncated_text() -> bool {
    true
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            context_pressure: ContextPressurePolicy::default(),
            circuit: CircuitPolicy::default(),
            mark_truncated_text: true,
        }
    }
}

impl RuntimePolicy {
    /// Evaluate `facts` and return structured advisory evidence.
    ///
    /// Evidence is derived in priority order. Stall is the highest-priority
    /// signal (returns immediately); after that, pressure guidance, diagnostic guidance,
    /// phase transition, and expansion signals are evaluated. Multiple items may
    /// be returned (e.g. pressure guidance + diagnostic signal).
    ///
    /// The caller may serialize these items for the model or telemetry, but
    /// must not apply them as runtime commands. The policy only reads the pure
    /// factual snapshot and never mutates internal state.
    pub fn decide(&self, facts: &JournalFacts) -> Vec<RuntimePolicyEvidence> {
        let mut evidence = Vec::new();

        // ── Priority 1: Stall Interrupt ──────────────────────────────────
        // Framework-detected tool-signature repetition. Short-circuit
        // immediately — stall overrides all other decisions.
        if let Some(ref reason) = facts.stall.stall_reason {
            evidence.push(RuntimePolicyEvidence::Advisory {
                message: format!(
                    "Stall detected: {}. Consider changing your approach or using a different tool.",
                    reason
                ),
            });
            return evidence;
        }

        // ── Priority 2: Context Pressure Guidance ────────────────────────
        // Aggressive first (higher severity); Normal fallback.
        if facts.performance.token_pressure >= self.context_pressure.aggressive_pressure_threshold {
            evidence.push(RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Aggressive,
            });
        } else if facts.performance.token_pressure >= self.context_pressure.pressure_threshold {
            evidence.push(RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            });
        }

        // ── Priority 3: Circuit Breaker Guidance ─────────────────────────
        //
        // First principle: a large task can spend many rounds gathering
        // evidence. Read-only streaks and zero-outcome streaks are risks to
        // manage, not proof that the runtime should shrink the hard budget.
        // Shrinking `max_turns` here caused long investigations to cliff into
        // empty_completion after the model kept using tools. Emit an actionable
        // adjustment signal instead and let the existing hard-stop paths handle
        // true runaway failures.
        let mut circuit_reasons = Vec::new();
        if facts.performance.current_error_rate > self.circuit.error_rate_threshold {
            circuit_reasons.push(format!(
                "tool error rate is {:.0}%",
                facts.performance.current_error_rate * 100.0
            ));
        }
        if facts.streaks.consecutive_read_only > self.circuit.max_consecutive_reads {
            circuit_reasons.push(format!(
                "{} consecutive read-only rounds",
                facts.streaks.consecutive_read_only
            ));
        }
        if facts.streaks.consecutive_rounds_without_outcome > self.circuit.max_consecutive_errors {
            circuit_reasons.push(format!(
                "{} consecutive rounds without observable outcome",
                facts.streaks.consecutive_rounds_without_outcome
            ));
        }
        if !circuit_reasons.is_empty() {
            evidence.push(RuntimePolicyEvidence::Advisory {
                message: format!(
                    "Circuit-breaker risk detected ({}). Do not stop solely because of this. Adjust strategy: summarize evidence gathered so far, state the next hypothesis, and either run one targeted experiment or produce a direct answer if enough evidence is available.",
                    circuit_reasons.join(", ")
                ),
            });
        }

        // ── Priority 4: Budget Expansion ─────────────────────────────────
        // Agent consistently producing outcomes and budget is getting tight.
        // Do not wait for the halfway cliff: the model has shown useful
        // progress, so framework budget should create room before it starts
        // self-pacing around scarcity instead of solving the task.
        let expansion_threshold = facts.budget.budget_max.saturating_mul(3) / 4;
        if facts.streaks.consecutive_rounds_with_outcome >= self.expand_after_consecutive_outcomes
            && facts.budget.budget_remaining <= expansion_threshold
        {
            evidence.push(RuntimePolicyEvidence::BudgetExpansionSuggested {
                factor: self.expand_factor,
                max_ceiling: self.max_ceiling,
            });
        }

        // ── Priority 6: Zero-streak Signal ───────────────────────────────
        // Agent stuck with zero outcomes too long.
        if facts.streaks.consecutive_rounds_without_outcome >= self.reflect_after_consecutive_zero {
            evidence.push(RuntimePolicyEvidence::Advisory {
                message: format!(
                    "{} consecutive rounds without observable progress. Consider pausing to reflect on whether your approach is effective.",
                    facts.streaks.consecutive_rounds_without_outcome
                ),
            });
        }

        if evidence.is_empty() {
            evidence.push(RuntimePolicyEvidence::NoAdvisory);
        }

        evidence
    }

    /// Produce a truncation marker when the model's output was cut off by the
    /// API token limit (`finish_reason == "length"`). Returns `Some(marker)`
    /// when the policy is enabled and the reason signals truncation; `None`
    /// otherwise (prevents noise for normal stop/tool_calls).
    pub fn truncation_marker(&self, finish_reason: Option<&str>) -> Option<&'static str> {
        if !self.mark_truncated_text {
            return None;
        }
        if finish_reason == Some("length") {
            Some("[truncated — output cut off by token limit]")
        } else {
            None
        }
    }
}

const POLICY_RECORD_WINDOW: usize = 64;

/// Incremental state for the canonical behavioral-feedback evaluator.
/// The bounded record window supports structured overlap/family detectors;
/// cumulative counters remain scalar and no request scans journal history.
#[derive(Default)]
pub struct RuntimePolicyEvaluationState {
    revision: u32,
    subject: Option<RuntimePolicySubject>,
    records_cursor: usize,
    /// Run-wide bounded evidence. Failures and rejected requests are retained
    /// because they are precisely the facts the feedback loop must diagnose.
    record_window: VecDeque<ToolCallRecord>,
    /// Evidence local to the active Work item. This resets when ownership
    /// moves to another item so healthy decomposition does not look like one
    /// endlessly wandering investigation.
    subject_record_window: VecDeque<ToolCallRecord>,
    /// Operation-scoped causes that made low-yield evidence strong enough to
    /// affect scheduling. Window aging never clears these; only a successful
    /// execution of the same normalized operation identity does. Keeping the
    /// identity below the tool-name level prevents an unrelated `bash` (or
    /// other multiplexed tool) success from masquerading as recovery.
    low_yield_problem_operations: BTreeSet<String>,
    prior_active_failure_operations: BTreeSet<String>,
    prior_active_rejected_operations: BTreeSet<String>,
    /// Number of inspection-only authoritative boundaries observed after a
    /// SearchFanout Converge advisory. `None` means no active advisory watch.
    search_converged_followup_inspections: Option<u8>,
    latest: RuntimePolicyFeedbackSet,
}

impl RuntimePolicyEvaluationState {
    #[must_use]
    pub fn latest(&self) -> &RuntimePolicyFeedbackSet {
        &self.latest
    }
}

/// Advance runtime policy exactly once after authoritative tool outcomes have
/// entered `records`. `None` means no semantic revision: request retries and
/// non-executed outcomes must reuse the previous bytes unchanged.
pub fn evaluate_tool_boundary(
    state: &mut RuntimePolicyEvaluationState,
    subject: RuntimePolicySubject,
    records: &[ToolCallRecord],
    completed_rounds: u32,
) -> Option<RuntimePolicyFeedbackSet> {
    evaluate_tool_boundary_with_thresholds(
        state,
        subject,
        records,
        completed_rounds,
        astra_turn_core::evaluation::EvaluationThresholds::default(),
    )
}

/// Production entry point. Runtime feedback and terminal evaluation share the
/// same calibrated detector thresholds, so Desktop/Introspect cannot disagree
/// with the eventual durable turn assessment merely because they used
/// different configuration authorities.
pub fn evaluate_tool_boundary_with_thresholds(
    state: &mut RuntimePolicyEvaluationState,
    subject: RuntimePolicySubject,
    records: &[ToolCallRecord],
    completed_rounds: u32,
    thresholds: astra_turn_core::evaluation::EvaluationThresholds,
) -> Option<RuntimePolicyFeedbackSet> {
    if records.len() < state.records_cursor {
        *state = RuntimePolicyEvaluationState::default();
    }

    let subject_changed = state
        .subject
        .as_ref()
        .is_some_and(|prior| prior != &subject);
    if subject_changed {
        state.subject = Some(subject.clone());
        state.subject_record_window.clear();
        state.low_yield_problem_operations.clear();
        state.prior_active_failure_operations.clear();
        state.prior_active_rejected_operations.clear();
        state.search_converged_followup_inspections = None;
    }
    if state.subject.is_none() {
        state.subject = Some(subject.clone());
    }

    let new_records = &records[state.records_cursor..];
    state.records_cursor = records.len();
    let lifecycle_transition = new_records.iter().any(|record| {
        is_work_lifecycle_tool(&record.name)
            && record.effective_disposition() == ToolCallDisposition::Executed
            && record.ok
    });
    let mut authoritative_observation = false;
    for record in new_records {
        if is_work_lifecycle_tool(&record.name) {
            continue;
        }
        if !matches!(
            record.effective_disposition(),
            ToolCallDisposition::Executed | ToolCallDisposition::Rejected
        ) {
            continue;
        }
        authoritative_observation = true;
        state.record_window.push_back(record.clone());
        state.subject_record_window.push_back(record.clone());
        while state.record_window.len() > POLICY_RECORD_WINDOW {
            state.record_window.pop_front();
        }
        while state.subject_record_window.len() > POLICY_RECORD_WINDOW {
            state.subject_record_window.pop_front();
        }
    }
    if lifecycle_transition {
        state.search_converged_followup_inspections = None;
    }
    if !authoritative_observation && !subject_changed && !lifecycle_transition {
        return None;
    }

    let run_window = state.record_window.iter().cloned().collect::<Vec<_>>();
    let subject_window = state
        .subject_record_window
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    // Keep the run-wide window lossless for failure/rejection diagnostics, but
    // only successful terminal executions are behavioral evidence.  A tool
    // call can be `Executed` while `ok == false`; feeding that record to the
    // exploration/search/validation detectors would turn an observed failure
    // into positive evidence about the agent's strategy.  That is especially
    // harmful for failed mutations, which must not invalidate a healthy read
    // projection.  Terminal audit may retain failed attempts for failure
    // metrics; this narrower success boundary is specifically for online
    // behavioral guidance so a failed attempt cannot steer the next request.
    let successful_subject_window = subject_window
        .iter()
        .filter(|record| {
            record.effective_disposition() == ToolCallDisposition::Executed && record.ok
        })
        .cloned()
        .collect::<Vec<_>>();
    let redundant_reads = astra_turn_core::evaluation::count_active_redundant_overlapping_reads(
        &successful_subject_window,
    );
    let exploration_streak =
        astra_turn_core::evaluation::exploration_family_round_streak(&successful_subject_window)
            .map(|(_, streak)| streak)
            .unwrap_or(0);
    let sequential_single_call_streak = trailing_single_tool_round_streak(&subject_window);
    let unresolved_outcomes =
        astra_turn_core::evaluation::count_unresolved_tool_outcome_failures(&run_window);
    let active_failure_operations =
        astra_turn_core::evaluation::active_execution_failure_operation_keys(&run_window);
    let active_rejected_operations =
        astra_turn_core::evaluation::active_rejected_operation_keys(&run_window);
    let rejected_requests = active_rejected_operations.len();
    // Resolve sticky scheduler pressure only with tool-scoped authoritative
    // recovery. Invocation shape (batching, a mutation, or changed arguments)
    // is not outcome evidence and cannot clear a prior convergence decision.
    for record in new_records {
        if record.effective_disposition() == ToolCallDisposition::Executed && record.ok {
            for key in astra_turn_core::evaluation::tool_outcome_recovery_keys(record) {
                state.low_yield_problem_operations.remove(&key);
            }
            if let Some(key) = astra_turn_core::evaluation::rejected_operation_key(record) {
                state.low_yield_problem_operations.remove(&key);
            }
        }
    }
    let validation_retries =
        astra_turn_core::evaluation::max_redundant_validation_retries(&successful_subject_window);
    let search_fanout =
        astra_turn_core::evaluation::count_search_fanout(&successful_subject_window);
    let successful_new_records = new_records
        .iter()
        .filter(|record| {
            !is_work_lifecycle_tool(&record.name)
                && record.effective_disposition() == ToolCallDisposition::Executed
                && record.ok
        })
        .cloned()
        .collect::<Vec<_>>();
    let new_successful_searches =
        astra_turn_core::evaluation::count_search_fanout(&successful_new_records);
    let authoritative_boundary_records = new_records
        .iter()
        .filter(|record| {
            !is_work_lifecycle_tool(&record.name)
                && matches!(
                    record.effective_disposition(),
                    ToolCallDisposition::Executed | ToolCallDisposition::Rejected
                )
        })
        .collect::<Vec<_>>();
    let boundary_is_inspection_only = !authoritative_boundary_records.is_empty()
        && authoritative_boundary_records
            .iter()
            .all(|record| record_is_observation_only(record));
    let boundary_has_decisive_transition = authoritative_boundary_records
        .iter()
        .any(|record| record_is_decisive_transition(record));
    let watch_will_be_ignored = state
        .search_converged_followup_inspections
        .is_some_and(|followups| boundary_is_inspection_only && followups >= 1);
    let general_entry_limit =
        RuntimePolicyFeedbackSet::MAX_ENTRIES.saturating_sub(usize::from(watch_will_be_ignored));
    let prior_entries = match &state.latest {
        RuntimePolicyFeedbackSet::Evaluated {
            subject: prior_subject,
            entries,
            ..
        } if prior_subject == &subject => entries.as_slice(),
        _ => &[],
    };
    let reobserved_failure_operations = new_records
        .iter()
        .filter(|record| {
            record.effective_disposition() == ToolCallDisposition::Executed
                && !record.ok
                && astra_turn_core::evaluation::tool_outcome_operation_key(record)
                    .is_some_and(|key| active_failure_operations.contains(&key))
                && state.prior_active_failure_operations.contains(
                    &astra_turn_core::evaluation::tool_outcome_operation_key(record)
                        .unwrap_or_default(),
                )
        })
        .filter_map(astra_turn_core::evaluation::tool_outcome_operation_key)
        .collect::<BTreeSet<_>>();
    let reobserved_rejected_operations = new_records
        .iter()
        .filter(|record| {
            record.effective_disposition() == ToolCallDisposition::Rejected
                && astra_turn_core::evaluation::rejected_operation_key(record)
                    .is_some_and(|key| active_rejected_operations.contains(&key))
                && state.prior_active_rejected_operations.contains(
                    &astra_turn_core::evaluation::rejected_operation_key(record)
                        .unwrap_or_default(),
                )
        })
        .filter_map(astra_turn_core::evaluation::rejected_operation_key)
        .collect::<BTreeSet<_>>();
    let signal_reobserved = |signal| match signal {
        RuntimePolicySignal::UnresolvedToolOutcomes => !reobserved_failure_operations.is_empty(),
        RuntimePolicySignal::RejectedToolRequests => !reobserved_rejected_operations.is_empty(),
        _ => false,
    };
    let stage = |signal, evidence_count: u32| {
        let Some(prior) = prior_entries.iter().find(|entry| entry.signal == signal) else {
            return RuntimePolicyStage::Observe;
        };
        if evidence_count < prior.evidence_count {
            return RuntimePolicyStage::Observe;
        }
        // Persistence becomes stronger only when this boundary re-observes the
        // same typed problem. An unrelated healthy tool call must not promote
        // stale evidence merely because another round elapsed.
        let recoverable_signal = matches!(
            signal,
            RuntimePolicySignal::UnresolvedToolOutcomes | RuntimePolicySignal::RejectedToolRequests
        );
        if recoverable_signal && signal_reobserved(signal) {
            RuntimePolicyStage::Converge
        } else if recoverable_signal {
            RuntimePolicyStage::Observe
        } else if prior.stage == RuntimePolicyStage::Converge
            || evidence_count > prior.evidence_count
        {
            RuntimePolicyStage::Converge
        } else {
            RuntimePolicyStage::Observe
        }
    };
    let mut entries = Vec::with_capacity(RuntimePolicyFeedbackSet::MAX_ENTRIES);
    if exploration_streak >= thresholds.exploration_family_churn
        && entries.len() < general_entry_limit
    {
        entries.push(RuntimePolicyFeedbackEntry {
            signal: RuntimePolicySignal::ExplorationFamilyChurn,
            stage: stage(
                RuntimePolicySignal::ExplorationFamilyChurn,
                saturating_u32(exploration_streak),
            ),
            observed_at_round: completed_rounds,
            evidence_count: saturating_u32(exploration_streak),
            recommendation: RuntimePolicyRecommendation::TestExactHypothesis,
        });
    }
    if redundant_reads >= thresholds.redundant_overlapping_reads
        && entries.len() < general_entry_limit
    {
        entries.push(RuntimePolicyFeedbackEntry {
            signal: RuntimePolicySignal::RedundantReads,
            stage: stage(
                RuntimePolicySignal::RedundantReads,
                saturating_u32(redundant_reads),
            ),
            observed_at_round: completed_rounds,
            evidence_count: saturating_u32(redundant_reads),
            recommendation: RuntimePolicyRecommendation::ReuseKnownContent,
        });
    }
    if unresolved_outcomes > 0 && entries.len() < general_entry_limit {
        entries.push(RuntimePolicyFeedbackEntry {
            signal: RuntimePolicySignal::UnresolvedToolOutcomes,
            stage: stage(
                RuntimePolicySignal::UnresolvedToolOutcomes,
                saturating_u32(unresolved_outcomes),
            ),
            observed_at_round: completed_rounds,
            evidence_count: saturating_u32(unresolved_outcomes),
            recommendation: RuntimePolicyRecommendation::DiagnoseToolOutcomes,
        });
    }
    if rejected_requests > 0 && entries.len() < general_entry_limit {
        entries.push(RuntimePolicyFeedbackEntry {
            signal: RuntimePolicySignal::RejectedToolRequests,
            stage: stage(
                RuntimePolicySignal::RejectedToolRequests,
                saturating_u32(rejected_requests),
            ),
            observed_at_round: completed_rounds,
            evidence_count: saturating_u32(rejected_requests),
            recommendation: RuntimePolicyRecommendation::RepairToolRequest,
        });
    }
    if validation_retries >= thresholds.redundant_validation_retries
        && entries.len() < general_entry_limit
    {
        entries.push(RuntimePolicyFeedbackEntry {
            signal: RuntimePolicySignal::ValidationRetryChurn,
            stage: stage(
                RuntimePolicySignal::ValidationRetryChurn,
                saturating_u32(validation_retries),
            ),
            observed_at_round: completed_rounds,
            evidence_count: saturating_u32(validation_retries),
            recommendation: RuntimePolicyRecommendation::ChangeValidationStrategy,
        });
    }
    // Search volume is ambiguous in isolation, so it never acquires scheduler
    // authority.  It is still useful as early model feedback: waiting for an
    // unrelated failure to occur made healthy-but-meandering investigations
    // invisible until the failure itself happened.  The generic stage logic
    // keeps the first threshold crossing at Observe and advances only when a
    // later authoritative boundary contains additional search evidence.
    let entry_corroborates_low_yield = |entry: &RuntimePolicyFeedbackEntry| {
        let recoverable_signal = matches!(
            entry.signal,
            RuntimePolicySignal::UnresolvedToolOutcomes | RuntimePolicySignal::RejectedToolRequests
        );
        !recoverable_signal
            || entry.stage == RuntimePolicyStage::Converge
            || signal_reobserved(entry.signal)
    };
    let low_yield_slot_is_needed = watch_will_be_ignored
        || (completed_rounds as usize >= thresholds.llm_round_churn
            && (sequential_single_call_streak >= thresholds.llm_round_churn
                || entries
                    .iter()
                    .any(|entry| entry_corroborates_low_yield(entry))));
    let search_entry_limit = general_entry_limit.saturating_sub(usize::from(
        low_yield_slot_is_needed && !watch_will_be_ignored,
    ));
    let search_stage = (search_fanout >= thresholds.search_fanout).then(|| {
        stage(
            RuntimePolicySignal::SearchFanout,
            saturating_u32(search_fanout),
        )
    });
    let mut search_converge_was_projected = false;
    if let Some(search_stage) = search_stage
        && entries.len() < search_entry_limit
    {
        search_converge_was_projected = search_stage == RuntimePolicyStage::Converge;
        entries.push(RuntimePolicyFeedbackEntry {
            signal: RuntimePolicySignal::SearchFanout,
            stage: search_stage,
            observed_at_round: completed_rounds,
            evidence_count: saturating_u32(search_fanout),
            recommendation: RuntimePolicyRecommendation::NarrowEvidenceSearch,
        });
    }
    // A Converge advisory is useful only if the next request can change the
    // model's behavior. Permit one exact follow-up inspection, then surface a
    // stronger decision/synthesis recommendation when a second authoritative
    // boundary remains exclusively observational. This is still advisory:
    // it changes no budget, tool admission, or terminal authority.
    //
    // Any typed action boundary resets the watch. It can re-arm only when a
    // later boundary both contributes new search evidence and actually
    // projects a Converge entry to the model. Introspect/reflect observe runtime
    // state and deliberately do not masquerade as task progress.
    if boundary_has_decisive_transition || lifecycle_transition {
        state.search_converged_followup_inspections = None;
    } else if let Some(followups) = state.search_converged_followup_inspections.as_mut() {
        if boundary_is_inspection_only {
            *followups = followups.saturating_add(1);
        }
    } else if search_converge_was_projected && new_successful_searches > 0 {
        state.search_converged_followup_inspections = Some(0);
    }
    let ignored_search_advisory = state
        .search_converged_followup_inspections
        .is_some_and(|followups| followups >= 2);
    // Long tasks are healthy. Round count becomes actionable only when a
    // separate low-yield signal is already present. A long trailing sequence
    // of one executed call per round is itself such a typed cadence fact, but
    // cadence alone remains Observe: legitimate serial work must not lose its
    // budget merely for being serial. Changing arguments, observing a failure,
    // or writing scratch state does not demonstrate a strategy change. Only an
    // persistent, recoverable tool-outcome fact can advance it to Converge.
    // Once the correlated signal has reached Converge for this subject, keep that decision stable: a
    // sliding detector window forgetting the original corroborating record is
    // not evidence that the trajectory recovered. Subject changes reset the
    // evaluator, and the guidance still permits one materially different
    // decisive check before synthesis.
    let prior_low_yield_converged = prior_entries.iter().any(|entry| {
        entry.signal == RuntimePolicySignal::LowYieldRoundChurn
            && entry.stage == RuntimePolicyStage::Converge
    });
    let cadence_observed = sequential_single_call_streak >= thresholds.llm_round_churn;
    // Two weak observations are not a strong stop signal. Cadence may advance
    // only when a recoverable tool-outcome fact was already Converge and
    // remains Converge at this boundary. Other advisory families remain useful
    // model guidance but cannot silently acquire scheduler authority.
    let persistent_recoverable_corroborator = entries.iter().any(|entry| {
        matches!(
            entry.signal,
            RuntimePolicySignal::UnresolvedToolOutcomes | RuntimePolicySignal::RejectedToolRequests
        ) && entry.stage == RuntimePolicyStage::Converge
            && prior_entries.iter().any(|prior| {
                prior.signal == entry.signal && prior.stage == RuntimePolicyStage::Converge
            })
    });
    let cadence_is_corroborated = cadence_observed && persistent_recoverable_corroborator;
    if cadence_is_corroborated {
        state
            .low_yield_problem_operations
            .extend(reobserved_failure_operations.iter().cloned());
        state
            .low_yield_problem_operations
            .extend(reobserved_rejected_operations.iter().cloned());
    }
    let sticky_convergence =
        prior_low_yield_converged && !state.low_yield_problem_operations.is_empty();
    // An old recoverable failure remains useful diagnostic evidence, but it
    // cannot by itself turn later healthy work into synthesis pressure.  A
    // recoverable signal corroborates low-yield work only when this boundary
    // observes the same operation again (which normally advances the signal
    // to Converge) or the projected signal is already Converge. Other typed
    // advisory families keep their existing low-yield behavior.
    let low_yield_corroborator_present = entries
        .iter()
        .any(|entry| entry_corroborates_low_yield(entry));
    if (completed_rounds as usize >= thresholds.llm_round_churn || ignored_search_advisory)
        && (cadence_observed
            || low_yield_corroborator_present
            || sticky_convergence
            || ignored_search_advisory)
        && entries.len() < RuntimePolicyFeedbackSet::MAX_ENTRIES
    {
        entries.push(RuntimePolicyFeedbackEntry {
            signal: RuntimePolicySignal::LowYieldRoundChurn,
            stage: if sticky_convergence || cadence_is_corroborated || ignored_search_advisory {
                RuntimePolicyStage::Converge
            } else {
                RuntimePolicyStage::Observe
            },
            observed_at_round: completed_rounds,
            evidence_count: completed_rounds,
            recommendation: RuntimePolicyRecommendation::SynthesizeAndDecide,
        });
    }
    state.prior_active_failure_operations = active_failure_operations;
    state.prior_active_rejected_operations = active_rejected_operations;
    publish_policy_set(state, subject, completed_rounds, entries)
}

/// Count trailing provider rounds that each executed exactly one tool call.
///
/// This deliberately ignores tool names and arguments.  It detects cadence,
/// not a scenario: changing a shell command every round must not evade the
/// same advisory that already exists for single-step execution. A failed call
/// or scratch mutation is still one model/tool round and therefore cannot
/// masquerade as a cadence recovery. A batched round, non-executed request, or
/// missing round identity breaks the streak. The result remains advisory-only.
fn trailing_single_tool_round_streak(records: &[ToolCallRecord]) -> usize {
    let mut index = records.len();
    let mut streak = 0;

    while index > 0 {
        let Some(round) = records[index - 1].round else {
            break;
        };
        let end = index;
        while index > 0 && records[index - 1].round == Some(round) {
            index -= 1;
        }
        let round_records = &records[index..end];
        if round_records.len() != 1 {
            break;
        }
        let record = &round_records[0];
        if record.effective_disposition() != ToolCallDisposition::Executed {
            break;
        }
        streak += 1;
    }

    streak
}

fn publish_policy_set(
    state: &mut RuntimePolicyEvaluationState,
    subject: RuntimePolicySubject,
    evaluated_at_round: u32,
    entries: Vec<RuntimePolicyFeedbackEntry>,
) -> Option<RuntimePolicyFeedbackSet> {
    let next_revision = match state.revision.checked_add(1) {
        Some(revision) => revision,
        None => {
            state.latest = RuntimePolicyFeedbackSet::NotEvaluated;
            return Some(state.latest.clone());
        }
    };
    let candidate = RuntimePolicyFeedbackSet::Evaluated {
        schema_version: RuntimePolicyFeedbackSet::SCHEMA_VERSION,
        revision: next_revision,
        evaluated_at_round,
        subject,
        entries,
    };
    if policy_semantically_equal(&state.latest, &candidate) {
        return None;
    }
    state.revision = next_revision;
    state.latest = candidate.clone();
    Some(candidate)
}

fn policy_semantically_equal(
    left: &RuntimePolicyFeedbackSet,
    right: &RuntimePolicyFeedbackSet,
) -> bool {
    match (left, right) {
        (
            RuntimePolicyFeedbackSet::Evaluated {
                subject: left_subject,
                entries: left_entries,
                ..
            },
            RuntimePolicyFeedbackSet::Evaluated {
                subject: right_subject,
                entries: right_entries,
                ..
            },
        ) => {
            left_subject == right_subject
                && left_entries.len() == right_entries.len()
                && left_entries.iter().zip(right_entries).all(|(left, right)| {
                    left.signal == right.signal
                        && left.stage == right.stage
                        && left.evidence_count == right.evidence_count
                        && left.recommendation == right.recommendation
                })
        }
        (RuntimePolicyFeedbackSet::NotEvaluated, RuntimePolicyFeedbackSet::NotEvaluated) => true,
        _ => false,
    }
}

fn is_work_lifecycle_tool(name: &str) -> bool {
    matches!(
        name,
        "start_work" | "run_next_work_item" | "settle_work_item"
    )
}

fn record_args_value(record: &ToolCallRecord) -> Option<serde_json::Value> {
    record
        .authoritative_args_full()
        .and_then(|args| serde_json::from_str(args).ok())
}

/// Positive typed classification of another observation boundary.
///
/// This deliberately reuses the canonical tool metadata instead of assuming
/// every tool outside the narrow search/read detector is task progress. It
/// therefore covers directory, Git, shell, network, and code-intelligence
/// readers without a second scenario vocabulary.
fn record_is_observation_only(record: &ToolCallRecord) -> bool {
    // Execution-owned outcome evidence outranks the admission-time command
    // classifier. Some otherwise observational command families have
    // argument forms that write; a bound, validated changed receipt proves
    // this invocation crossed a task-state boundary without teaching the
    // policy engine a second shell vocabulary.
    if crate::turn::agentic_loop::execution_phase::record_has_trusted_workspace_mutation_receipt(
        record,
    ) {
        return false;
    }
    if matches!(record.name.as_str(), "introspect" | "reflect") {
        return true;
    }
    let args = record_args_value(record);
    astra_turn_core::tool::categories::classify(&record.name, args.as_ref())
        .category
        .is_read_only()
}

/// Positive typed classification of a task-course transition.
///
/// Canonical validation is a transition even when the underlying command is
/// safe/read-only. Mutating, shell, and consultative categories are otherwise
/// explicit non-observation actions. Unknown or malformed shell arguments stay
/// conservative; a rejected/read-only call cannot become action merely because
/// it is absent from an exploration allowlist.
fn record_is_decisive_transition(record: &ToolCallRecord) -> bool {
    // A policy transition is a successful, typed change in task course.  A
    // failed scratch command must not erase convergence pressure merely
    // because its tool category can write.  Keep this narrower than general
    // admission: the policy is evidence for user-visible progress, not a
    // capability classifier.
    if !record.was_executed() || !record.ok {
        return false;
    }
    if crate::turn::agentic_loop::execution_phase::record_has_trusted_workspace_mutation_receipt(
        record,
    ) || crate::turn::agentic_loop::lifecycle::record_has_typed_workspace_tool_receipt(record)
    {
        return true;
    }
    if matches!(record.name.as_str(), "introspect" | "reflect") {
        return false;
    }
    if record.authoritative_args_full().is_some_and(|args| {
        astra_turn_core::evaluation::normalize_validation_prefix(&record.name, args).is_some()
    }) {
        return true;
    }
    is_work_lifecycle_tool(&record.name)
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Whether the persisted policy evidence has crossed the low-yield
/// convergence boundary for its current typed subject.
///
/// The feedback itself remains advisory to the model. This predicate is kept
/// as a typed projection for telemetry and callers that want to explain why a
/// synthesis recommendation was emitted; it must not be used as an execution
/// veto by the scheduler.
pub fn feedback_requires_convergence(set: &RuntimePolicyFeedbackSet) -> bool {
    let RuntimePolicyFeedbackSet::Evaluated { entries, .. } = set else {
        return false;
    };
    entries.iter().any(|entry| {
        entry.signal == RuntimePolicySignal::LowYieldRoundChurn
            && entry.stage == RuntimePolicyStage::Converge
    })
}

/// Whether a terminal answer deserves one bounded evidence-reconciliation
/// pass before it becomes user-visible.
///
/// A single failed command is often the evidence the user asked for, so it
/// must not force another model call.  This boundary is intentionally narrower:
/// the same unresolved-outcome fact must survive long enough to reach the
/// evaluator's `Converge` stage.  Recovered failures disappear from the active
/// projection and therefore never satisfy this predicate.
pub fn feedback_requires_outcome_reconciliation(set: &RuntimePolicyFeedbackSet) -> bool {
    let RuntimePolicyFeedbackSet::Evaluated { entries, .. } = set else {
        return false;
    };
    entries.iter().any(|entry| {
        entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes
            && entry.stage == RuntimePolicyStage::Converge
    })
}

/// Stable Context Pipeline projection. It contains typed subject/evidence and
/// recommendations, never an execution command.
pub fn policy_advisory_payload(set: &RuntimePolicyFeedbackSet) -> Option<serde_json::Value> {
    let RuntimePolicyFeedbackSet::Evaluated {
        revision,
        subject,
        entries,
        ..
    } = set
    else {
        return None;
    };
    if entries.is_empty() {
        return None;
    }
    let projected_entries = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "signal": entry.signal,
                "stage": entry.stage,
                "observed_at_round": entry.observed_at_round,
                "evidence_count": entry.evidence_count,
                "recommendation": entry.recommendation,
                "instruction": recommendation_text(entry.recommendation, entry.stage),
            })
        })
        .collect::<Vec<_>>();
    Some(serde_json::json!({
        "schema": "runtime_policy_feedback.v2",
        "revision": revision,
        "subject": subject,
        "entries": projected_entries,
        "authority": "advisory_evidence_only",
    }))
}

fn recommendation_text(
    recommendation: RuntimePolicyRecommendation,
    stage: RuntimePolicyStage,
) -> &'static str {
    match (recommendation, stage) {
        (RuntimePolicyRecommendation::TestExactHypothesis, RuntimePolicyStage::Observe) => {
            "Exploration remains in one family. State the unresolved hypothesis and test it directly instead of repeating the family by default."
        }
        (RuntimePolicyRecommendation::TestExactHypothesis, RuntimePolicyStage::Converge) => {
            "The same exploration family persisted after prior feedback. Stop repeating it; use the evidence already present to decide the hypothesis, or run one materially different decisive check."
        }
        (RuntimePolicyRecommendation::ReuseKnownContent, RuntimePolicyStage::Observe) => {
            "Overlapping unchanged content is already available. Reuse it, or read only a precise unseen range when that range is the named evidence gap."
        }
        (RuntimePolicyRecommendation::ReuseKnownContent, RuntimePolicyStage::Converge) => {
            "Overlapping reads persisted after prior feedback. Do not reread known content; decide from it or inspect only one precise unseen range that directly resolves the active subject."
        }
        (RuntimePolicyRecommendation::DiagnoseToolOutcomes, RuntimePolicyStage::Observe) => {
            "Multiple typed tool outcomes remain unresolved. Separate environment/capability failure from product failure; if the same operation has failed again, stop trying adjacent variants, use a known-good alternative or introspect live tool health, then resolve the exact failure. While a Work item has unresolved execution evidence, do not settle it as delivered: continue with a materially different repair or validation step, or settle blocked/failed with the verified limiting fact."
        }
        (RuntimePolicyRecommendation::DiagnoseToolOutcomes, RuntimePolicyStage::Converge) => {
            "Tool outcomes remain unresolved after prior feedback. Stop retrying adjacent variants; use introspect for the live failure pattern, then change the execution path. Do not settle an active Work item as delivered until the relevant failure is resolved by direct evidence; if it cannot be resolved, settle blocked/failed with the verified limiting fact."
        }
        (RuntimePolicyRecommendation::RepairToolRequest, RuntimePolicyStage::Observe) => {
            "Several tool requests were rejected before execution. Re-read the visible schema and repair the exact arguments or authority boundary before issuing another request."
        }
        (RuntimePolicyRecommendation::RepairToolRequest, RuntimePolicyStage::Converge) => {
            "Rejected requests persisted after prior feedback. Stop guessing at the call shape; inspect the live trace/schema and make one contract-valid request."
        }
        (RuntimePolicyRecommendation::NarrowEvidenceSearch, RuntimePolicyStage::Observe) => {
            "Search fan-out reached the advisory threshold. Summarize what is already known, name the remaining evidence gap, and run one narrow query for that gap. This is guidance, not evidence that the task should stop."
        }
        (RuntimePolicyRecommendation::NarrowEvidenceSearch, RuntimePolicyStage::Converge) => {
            "Search fan-out persisted after prior feedback. Stop broad discovery; decide from collected evidence or inspect one exact unresolved location."
        }
        (RuntimePolicyRecommendation::ChangeValidationStrategy, RuntimePolicyStage::Observe) => {
            "The same validation family failed repeatedly without an intervening change. Diagnose the environment or prerequisite once before choosing a materially different validation path."
        }
        (RuntimePolicyRecommendation::ChangeValidationStrategy, RuntimePolicyStage::Converge) => {
            "Validation retry churn persisted. Do not rerun equivalent checks; use authoritative CI/artifacts or fix the prerequisite, and state the resulting confidence boundary."
        }
        (RuntimePolicyRecommendation::SynthesizeAndDecide, RuntimePolicyStage::Observe) => {
            "Low-yield rounds detected. Name the leading hypothesis and one falsifier internally, reuse the evidence already collected, then take a decisive action that closes a still-unmet user predicate. For an authorized change, make the needed mutation before running the complete unmodified project acceptance harness from a fresh process. Do not repeat equivalent probes or narrate the plan back to the user."
        }
        (RuntimePolicyRecommendation::SynthesizeAndDecide, RuntimePolicyStage::Converge) => {
            "Low-yield work persisted after prior feedback. Stop new exploration and stop restating the plan. Use the evidence now: complete the remaining authorized mutation, run the complete unmodified acceptance harness after the final mutation, or answer with the exact unresolved boundary. Any further tool call must directly close a named user predicate and must not repeat an existing probe."
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn work_subject(item: &str) -> RuntimePolicySubject {
        RuntimePolicySubject::WorkItem {
            attempt_id: format!("attempt-{item}"),
            item_id: item.to_string(),
            item_revision: 1,
            objective: format!("Inspect {item}"),
            expected_result: format!("Verified {item}"),
        }
    }

    fn executed(name: &str, args: &str, round: u32) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            args_full: Some(args.to_string()),
            round: Some(round),
            disposition: Some(ToolCallDisposition::Executed),
            ok: true,
            ..Default::default()
        }
    }

    fn failed(name: &str, args: &str, round: u32, result_class: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            args_full: Some(args.to_string()),
            round: Some(round),
            disposition: Some(ToolCallDisposition::Executed),
            ok: false,
            result_class: Some(result_class.to_string()),
            ..Default::default()
        }
    }

    fn typed_writer(path: &str, round: u32) -> ToolCallRecord {
        let mut record = executed(
            "write_file",
            &format!(r#"{{"path":"{path}","content":"updated"}}"#),
            round,
        );
        let receipt = astra_tools::workspace_observation::typed_workspace_tool_receipt();
        record.workspace_mutation_observed = Some(true);
        record.workspace_mutation_scope =
            Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
        record.workspace_mutation_receipt = receipt
            .get(astra_tools::workspace_observation::RECEIPT_FIELD)
            .cloned();
        record
    }

    fn entries(set: &RuntimePolicyFeedbackSet) -> &[RuntimePolicyFeedbackEntry] {
        let RuntimePolicyFeedbackSet::Evaluated { entries, .. } = set else {
            panic!("evaluated feedback expected");
        };
        entries
    }

    #[test]
    fn canonical_policy_advances_only_on_new_authoritative_terminal_evidence() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let mut records = vec![executed("read_file", r#"{"path":"a.rs"}"#, 1)];
        let first = evaluate_tool_boundary(&mut state, subject.clone(), &records, 1)
            .expect("initial evaluation");
        assert!(matches!(
            first,
            RuntimePolicyFeedbackSet::Evaluated { revision: 1, .. }
        ));
        assert!(
            evaluate_tool_boundary(&mut state, subject.clone(), &records, 1).is_none(),
            "request preparation/retry without a terminal tool fact must reuse exact bytes"
        );

        records.push(executed("read_file", r#"{"path":"a.rs"}"#, 3));
        records.push(executed("read_file", r#"{"path":"a.rs"}"#, 3));
        records.push(executed("read_file", r#"{"path":"a.rs"}"#, 3));
        let converging = evaluate_tool_boundary(&mut state, subject, &records, 3)
            .expect("calibrated redundant-read evidence changes policy");
        let RuntimePolicyFeedbackSet::Evaluated {
            revision, entries, ..
        } = converging
        else {
            panic!("evaluated feedback expected");
        };
        assert_eq!(revision, 2);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].signal, RuntimePolicySignal::RedundantReads);
        assert_eq!(entries[0].stage, RuntimePolicyStage::Observe);

        records.push(executed("read_file", r#"{"path":"a.rs"}"#, 4));
        let converged = evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 4)
            .expect("continued redundant reads advance an observed signal to converge");
        assert!(matches!(
            converged,
            RuntimePolicyFeedbackSet::Evaluated { revision: 3, .. }
        ));

        records.push(executed("str_replace", r#"{"path":"a.rs"}"#, 5));
        assert_eq!(
            astra_turn_core::evaluation::count_active_redundant_overlapping_reads(&records),
            0,
            "the shared calibrated detector must treat mutation as invalidation"
        );
        let resolved = evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 5)
            .expect("a workspace mutation invalidates stale read-overlap evidence");
        assert!(matches!(
            resolved,
            RuntimePolicyFeedbackSet::Evaluated {
                revision: 4,
                ref entries,
                ..
            } if entries.is_empty()
        ));

        records.push(executed("read_file", r#"{"path":"d.rs"}"#, 6));
        assert!(
            evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 6).is_none(),
            "stable resolved evidence must reuse the exact policy revision"
        );
    }

    #[test]
    fn typed_failures_are_feedback_evidence_and_later_success_resolves_them() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let records = vec![
            failed("bash", r#"{"command":"check-a"}"#, 1, "env_failure"),
            failed("bash", r#"{"command":"check-b"}"#, 2, "execution_error"),
        ];
        let observed = evaluate_tool_boundary(&mut state, subject.clone(), &records, 2)
            .expect("failed terminal outcomes must advance policy evidence");
        assert!(entries(&observed).iter().any(|entry| {
            entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes
                && entry.recommendation == RuntimePolicyRecommendation::DiagnoseToolOutcomes
        }));

        let mut recovered = records;
        recovered.push(executed("bash", r#"{"command":"check-a"}"#, 3));
        recovered.last_mut().unwrap().result_class = Some("success".to_string());
        recovered.push(executed("bash", r#"{"command":"check-b"}"#, 3));
        recovered.last_mut().unwrap().result_class = Some("success".to_string());
        let resolved = evaluate_tool_boundary(&mut state, subject, &recovered, 3)
            .expect("authoritative success must clear stale failure feedback");
        assert!(
            !entries(&resolved)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
        );
    }

    #[test]
    fn failed_executions_do_not_become_behavioral_evidence() {
        let mut state = RuntimePolicyEvaluationState::default();
        let mut records = Vec::new();

        // These records deliberately satisfy the shapes that would otherwise
        // trigger every behavior detector: homogeneous exploration rounds,
        // search fan-out, repeated validation, and overlapping reads.  They
        // are all terminal failures, so the only valid online signal is the
        // unresolved-outcome diagnosis.
        for round in 1..=4 {
            records.push(failed(
                "grep",
                &format!(r#"{{"pattern":"failed-{round}"}}"#),
                round,
                "execution_error",
            ));
            records.push(failed(
                "rg",
                &format!(r#"{{"pattern":"failed-{round}-other"}}"#),
                round,
                "execution_error",
            ));
        }
        for round in 5..=7 {
            records.push(failed(
                "bash",
                r#"{"command":"cargo test -p astra-runtime"}"#,
                round,
                "execution_error",
            ));
        }
        for round in 8..=10 {
            records.push(failed(
                "read_file",
                r#"{"path":"src/lib.rs"}"#,
                round,
                "execution_error",
            ));
        }

        let mut thresholds = astra_turn_core::evaluation::EvaluationThresholds::default();
        // Keep this test focused on tool-shape detectors rather than the
        // independent long-trajectory synthesis nudge.
        thresholds.llm_round_churn = usize::MAX;
        let feedback = evaluate_tool_boundary_with_thresholds(
            &mut state,
            work_subject("item-1"),
            &records,
            10,
            thresholds,
        )
        .expect("failed terminal outcomes are authoritative evidence");

        let signals = entries(&feedback)
            .iter()
            .map(|entry| entry.signal)
            .collect::<Vec<_>>();
        assert!(signals.contains(&RuntimePolicySignal::UnresolvedToolOutcomes));
        for forbidden in [
            RuntimePolicySignal::RedundantReads,
            RuntimePolicySignal::ExplorationFamilyChurn,
            RuntimePolicySignal::ValidationRetryChurn,
            RuntimePolicySignal::SearchFanout,
            RuntimePolicySignal::LowYieldRoundChurn,
        ] {
            assert!(
                !signals.contains(&forbidden),
                "failed execution was treated as positive behavioral evidence: {forbidden:?}"
            );
        }
    }

    #[test]
    fn failed_mutation_does_not_invalidate_successful_read_evidence() {
        let mut state = RuntimePolicyEvaluationState::default();
        let mut records = vec![executed("read_file", r#"{"path":"src/lib.rs"}"#, 1)];
        records
            .extend((2..=4).map(|round| executed("read_file", r#"{"path":"src/lib.rs"}"#, round)));
        let first = evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 4)
            .expect("successful reads produce overlap evidence");
        assert!(
            entries(&first)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::RedundantReads)
        );

        records.push(failed(
            "str_replace",
            r#"{"path":"src/lib.rs"}"#,
            5,
            "execution_error",
        ));
        let after_failed_mutation =
            evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 5)
                .expect("the failed mutation is new failure evidence");
        assert!(
            entries(&after_failed_mutation)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::RedundantReads),
            "a failed mutation must not invalidate successful read evidence"
        );
    }

    #[test]
    fn unclassified_failure_is_feedback_and_matching_success_resolves_it() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let mut failed_call = failed("bash", r#"{"command":"check-a"}"#, 1, "");
        failed_call.result_class = None;
        let records = vec![failed_call];
        let observed = evaluate_tool_boundary(&mut state, subject.clone(), &records, 1)
            .expect("an unclassified governed failure must still advance policy evidence");
        assert!(entries(&observed).iter().any(|entry| {
            entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes
                && entry.recommendation == RuntimePolicyRecommendation::DiagnoseToolOutcomes
        }));

        let mut recovered = executed("bash", r#"{"command":"check-a"}"#, 2);
        recovered.result_class = None;
        let resolved =
            evaluate_tool_boundary(&mut state, subject, &[records[0].clone(), recovered], 2)
                .expect("a matching successful operation must update policy evidence");
        assert!(
            !entries(&resolved)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
        );
    }

    #[test]
    fn one_rejected_request_is_observed_and_persistence_drives_escalation() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let records = vec![ToolCallRecord {
            name: "read_file".to_string(),
            args_full: Some(r#"{"path":"src/lib.rs","start_line":9,"end_line":2}"#.to_string()),
            round: Some(1),
            disposition: Some(ToolCallDisposition::Rejected),
            ok: false,
            result_class: Some("rejected".to_string()),
            ..Default::default()
        }];

        let first = evaluate_tool_boundary(&mut state, subject.clone(), &records, 1)
            .expect("a typed rejection is actionable evidence");
        let first_entry = entries(&first)
            .iter()
            .find(|entry| entry.signal == RuntimePolicySignal::RejectedToolRequests)
            .expect("rejection feedback");
        assert_eq!(first_entry.stage, RuntimePolicyStage::Observe);

        let mut continued = records;
        continued.push(executed("grep", r#"{"pattern":"next"}"#, 2));
        assert!(
            evaluate_tool_boundary(&mut state, subject.clone(), &continued, 2).is_none(),
            "unrelated healthy evidence must not escalate or churn advisory bytes"
        );
        let latest_entry = entries(state.latest())
            .iter()
            .find(|entry| entry.signal == RuntimePolicySignal::RejectedToolRequests)
            .expect("recent rejection feedback");
        assert_eq!(latest_entry.stage, RuntimePolicyStage::Observe);

        continued.push(ToolCallRecord {
            name: "read_file".to_string(),
            args_full: Some(r#"{"path":"src/lib.rs","start_line":9,"end_line":2}"#.to_string()),
            round: Some(3),
            disposition: Some(ToolCallDisposition::Rejected),
            ok: false,
            result_class: Some("rejected".to_string()),
            ..Default::default()
        });
        let converged = evaluate_tool_boundary(&mut state, subject, &continued, 3)
            .expect("additional rejected evidence advances the stage");
        let converged_entry = entries(&converged)
            .iter()
            .find(|entry| entry.signal == RuntimePolicySignal::RejectedToolRequests)
            .expect("rejection feedback");
        assert_eq!(converged_entry.stage, RuntimePolicyStage::Converge);
    }

    #[test]
    fn search_volume_without_correlated_low_yield_evidence_is_observe_only() {
        let mut state = RuntimePolicyEvaluationState::default();
        let records = (0..astra_turn_core::evaluation::SEARCH_FANOUT_THRESHOLD)
            .map(|index| {
                executed(
                    "grep",
                    &format!(r#"{{"pattern":"independent-{index}"}}"#),
                    1,
                )
            })
            .collect::<Vec<_>>();

        let feedback = evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 1)
            .expect("new authoritative evidence");
        assert_eq!(
            entries(&feedback)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::SearchFanout)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe),
            "{feedback:?}"
        );
        assert!(!feedback_requires_convergence(&feedback));
    }

    #[test]
    fn long_batched_successful_tasks_do_not_trigger_round_churn_by_count_alone() {
        let mut state = RuntimePolicyEvaluationState::default();
        let records = (0..20)
            .flat_map(|round| {
                [
                    executed(
                        "probe_a",
                        &format!(r#"{{"query":"round-{round}-a"}}"#),
                        round,
                    ),
                    executed(
                        "probe_b",
                        &format!(r#"{{"query":"round-{round}-b"}}"#),
                        round,
                    ),
                ]
            })
            .collect::<Vec<_>>();
        let feedback = evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 20)
            .expect("new authoritative evidence");
        assert!(entries(&feedback).is_empty(), "{feedback:?}");
    }

    #[test]
    fn varying_single_call_rounds_become_auditable_low_yield_feedback() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let threshold = astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD as u32;
        let mut records = (1..=threshold)
            .map(|round| {
                executed(
                    "probe",
                    &format!(r#"{{"query":"distinct-{round}"}}"#),
                    round,
                )
            })
            .collect::<Vec<_>>();

        let observed = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold)
            .expect("threshold boundary must publish typed cadence evidence");
        assert_eq!(
            entries(&observed)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );

        records.push(executed(
            "probe",
            r#"{"query":"one-more-distinct-call"}"#,
            threshold + 1,
        ));
        let still_observed =
            evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 1)
                .expect("one extra single-call round remains in the grace window");
        assert_eq!(
            entries(&still_observed)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );

        let convergence_round = threshold + threshold.div_ceil(2);
        records.extend(
            ((threshold + 2)..=convergence_round)
                .map(|round| executed("probe", &format!(r#"{{"query":"grace-{round}"}}"#), round)),
        );
        let still_advisory =
            evaluate_tool_boundary(&mut state, subject, &records, convergence_round)
                .expect("cadence-only evidence remains advisory after the grace window");
        assert_eq!(
            entries(&still_advisory)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );
    }

    #[test]
    fn batch_breaks_cadence_but_failure_or_mutation_does_not_fake_recovery() {
        let threshold = astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD as u32;
        let mut records = (1..threshold)
            .map(|round| executed("probe", &format!(r#"{{"query":"{round}"}}"#), round))
            .collect::<Vec<_>>();
        records.push(executed("probe", r#"{"query":"a"}"#, threshold));
        records.push(executed("probe", r#"{"query":"b"}"#, threshold));
        assert_eq!(trailing_single_tool_round_streak(&records), 0);

        records.push(executed(
            "str_replace",
            r#"{"path":"src/lib.rs","old":"a","new":"b"}"#,
            threshold + 1,
        ));
        records.push(failed(
            "probe",
            r#"{"query":"failed"}"#,
            threshold + 2,
            "execution_error",
        ));
        assert_eq!(trailing_single_tool_round_streak(&records), 2);
    }

    #[test]
    fn short_single_call_sequence_remains_below_low_yield_threshold() {
        let mut state = RuntimePolicyEvaluationState::default();
        let records = (1..=6)
            .map(|round| executed("probe", &format!(r#"{{"query":"{round}"}}"#), round))
            .collect::<Vec<_>>();

        let feedback = evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 6)
            .expect("authoritative calls are evaluated");
        assert!(
            entries(&feedback)
                .iter()
                .all(|entry| entry.signal != RuntimePolicySignal::LowYieldRoundChurn)
        );
    }

    #[test]
    fn mixed_single_call_outcomes_require_persistent_corroboration_before_convergence() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let threshold = astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD as u32;
        let convergence_round = threshold + threshold.div_ceil(2);
        let mut records = (1..=threshold)
            .map(|round| {
                if round % 2 == 0 {
                    executed(
                        "str_replace",
                        &format!(r#"{{"path":"src/{round}.rs","old":"a","new":"b"}}"#),
                        round,
                    )
                } else {
                    failed(
                        "probe",
                        r#"{"query":"persistent"}"#,
                        round,
                        "execution_error",
                    )
                }
            })
            .collect::<Vec<_>>();

        let observed = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold)
            .expect("mixed single-call cadence is observed");
        assert_eq!(
            entries(&observed)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );

        records.extend(((threshold + 1)..=convergence_round).map(|round| {
            failed(
                "probe",
                r#"{"query":"persistent"}"#,
                round,
                "execution_error",
            )
        }));
        let corroborator_converged =
            evaluate_tool_boundary(&mut state, subject.clone(), &records, convergence_round)
                .expect("the independent failure signal first reaches convergence");
        assert_eq!(
            entries(&corroborator_converged)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe),
            "a newly converged corroborator does not immediately promote cadence"
        );

        records.push(failed(
            "probe",
            r#"{"query":"persistent"}"#,
            convergence_round + 1,
            "execution_error",
        ));
        let converged =
            evaluate_tool_boundary(&mut state, subject, &records, convergence_round + 1)
                .expect("persisted converged corroboration is re-evaluated");
        assert_eq!(
            entries(&converged)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Converge)
        );
    }

    #[test]
    fn late_transient_failure_does_not_promote_long_cadence_to_convergence() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let threshold = astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD as u32;
        let mut records = (1..=threshold + 5)
            .map(|round| executed("probe", &format!(r#"{{"query":"{round}"}}"#), round))
            .collect::<Vec<_>>();

        let observed = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 5)
            .expect("long serial cadence is visible");
        assert_eq!(
            entries(&observed)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );

        records.push(failed(
            "parser",
            r#"{"attempt":"new-hypothesis"}"#,
            threshold + 6,
            "execution_error",
        ));
        let after_failure =
            evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 6)
                .expect("the failed hypothesis is evaluated");
        assert_eq!(
            entries(&after_failure)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe),
            "one late failed hypothesis is an alert, not a scheduler verdict"
        );

        records.push(executed(
            "parser",
            r#"{"attempt":"new-hypothesis"}"#,
            threshold + 7,
        ));
        let recovered = evaluate_tool_boundary(&mut state, subject, &records, threshold + 7)
            .expect("the recovered failure is evaluated");
        assert_eq!(
            entries(&recovered)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );
        assert!(
            entries(&recovered)
                .iter()
                .all(|entry| entry.signal != RuntimePolicySignal::UnresolvedToolOutcomes)
        );
    }

    #[test]
    fn two_late_failures_then_same_tool_recovery_never_gain_scheduler_authority() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = (1..=13)
            .map(|round| executed("probe", &format!(r#"{{"step":{round}}}"#), round))
            .collect::<Vec<_>>();
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, 13);

        for round in 14..=15 {
            records.push(failed(
                "parser",
                r#"{"attempt":"persistent"}"#,
                round,
                "execution_error",
            ));
            let feedback = evaluate_tool_boundary(&mut state, subject.clone(), &records, round)
                .expect("late failure boundary is evaluated");
            assert!(
                !feedback_requires_convergence(&feedback),
                "weak or newly converged failure evidence needs another boundary"
            );
        }

        records.push(executed("parser", r#"{"attempt":"persistent"}"#, 16));
        let recovered = evaluate_tool_boundary(&mut state, subject, &records, 16)
            .expect("same-tool recovery is evaluated");
        assert!(!feedback_requires_convergence(&recovered));
        assert!(
            entries(&recovered)
                .iter()
                .all(|entry| entry.signal != RuntimePolicySignal::UnresolvedToolOutcomes)
        );
    }

    #[test]
    fn healthy_serial_run_after_mixed_first_round_stays_observe_only() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = vec![
            failed("probe", r#"{"mode":"unavailable"}"#, 1, "execution_error"),
            executed("probe", r#"{"mode":"fallback"}"#, 1),
        ];

        for round in 1..=14 {
            if round > 1 {
                records.push(executed("probe", &format!(r#"{{"step":{round}}}"#), round));
            }
            let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, round);
            assert!(
                !feedback_requires_convergence(state.latest()),
                "healthy serial progress must retain its execution budget at round {round}"
            );
        }
        let latest = state.latest();
        assert_eq!(
            entries(latest)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );
    }

    #[test]
    fn one_unreobserved_recoverable_failure_does_not_create_low_yield_pressure() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let threshold = astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD as u32;
        let mut records = vec![failed("bash", r#"{"command":"slow-check"}"#, 1, "timeout")];
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, 1);

        for round in 2..=threshold + 3 {
            records.push(executed(
                "probe_a",
                &format!(r#"{{"round":{round},"part":"a"}}"#),
                round,
            ));
            records.push(executed(
                "probe_b",
                &format!(r#"{{"round":{round},"part":"b"}}"#),
                round,
            ));
            let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, round);
            let feedback = state.latest();
            assert_eq!(
                entries(feedback)
                    .iter()
                    .find(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
                    .map(|entry| entry.stage),
                Some(RuntimePolicyStage::Observe),
                "one old failure remains diagnostic evidence"
            );
            assert!(
                entries(feedback)
                    .iter()
                    .all(|entry| entry.signal != RuntimePolicySignal::LowYieldRoundChurn),
                "one old, never-reobserved recoverable failure must not turn healthy rounds into synthesis pressure at round {round}"
            );
        }

        records.push(failed(
            "bash",
            r#"{"command":"slow-check"}"#,
            threshold + 4,
            "timeout",
        ));
        let reobserved = evaluate_tool_boundary(&mut state, subject, &records, threshold + 4)
            .expect("the same failed operation is reobserved");
        assert!(entries(&reobserved).iter().any(|entry| {
            entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes
                && entry.stage == RuntimePolicyStage::Converge
        }));
        assert!(
            entries(&reobserved)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
        );
    }

    #[test]
    fn search_fanout_warns_early_but_never_gains_scheduler_authority() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let threshold = astra_turn_core::evaluation::SEARCH_FANOUT_THRESHOLD as u32;
        let mut records = (1..=threshold)
            .map(|round| {
                executed(
                    "bash",
                    &format!(r#"{{"command":"rg needle-{round} src"}}"#),
                    round,
                )
            })
            .collect::<Vec<_>>();

        let observed = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold)
            .expect("threshold crossing must be visible without an unrelated failure");
        assert_eq!(
            entries(&observed)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::SearchFanout)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );
        assert!(!feedback_requires_convergence(&observed));

        records.push(executed(
            "read_file",
            r#"{"path":"src/lib.rs"}"#,
            threshold + 1,
        ));
        let unchanged =
            evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 1)
                .expect("the new non-search boundary updates cadence evidence");
        assert_eq!(
            entries(&unchanged)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::SearchFanout)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe),
            "elapsed rounds without new search evidence must not strengthen the warning"
        );

        records.push(executed(
            "bash",
            r#"{"command":"rg final-gap tests"}"#,
            threshold + 2,
        ));
        let persisted = evaluate_tool_boundary(&mut state, subject, &records, threshold + 2)
            .expect("additional search evidence advances the advisory");
        assert_eq!(
            entries(&persisted)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::SearchFanout)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Converge)
        );
        assert!(
            !feedback_requires_convergence(&persisted),
            "search shape alone is never an execution veto"
        );
    }

    #[test]
    fn ignored_search_convergence_gets_one_precise_inspection_before_decision_guidance() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let threshold = astra_turn_core::evaluation::SEARCH_FANOUT_THRESHOLD as u32;
        let mut records = (1..=threshold + 1)
            .map(|round| {
                executed(
                    "bash",
                    &format!(r#"{{"command":"rg question-{round} src"}}"#),
                    round,
                )
            })
            .collect::<Vec<_>>();

        let _ = evaluate_tool_boundary(
            &mut state,
            subject.clone(),
            &records[..threshold as usize],
            threshold,
        );
        let converged =
            evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 1)
                .expect("new search evidence converges the advisory");
        assert!(!feedback_requires_convergence(&converged));

        records.push(executed(
            "read_file",
            r#"{"path":"src/exact_gap.rs"}"#,
            threshold + 2,
        ));
        let one_precise_followup =
            evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 2)
                .expect("first precise inspection is evaluated");
        assert!(
            !feedback_requires_convergence(&one_precise_followup),
            "one exact follow-up after the advisory must remain allowed"
        );

        records.push(executed(
            "read_file",
            r#"{"path":"src/another_gap.rs"}"#,
            threshold + 3,
        ));
        let ignored = evaluate_tool_boundary(&mut state, subject, &records, threshold + 3)
            .expect("continued inspection after the allowance changes guidance");
        assert!(
            feedback_requires_convergence(&ignored),
            "a second inspection-only boundary should produce decision guidance"
        );
    }

    #[test]
    fn decisive_action_clears_ignored_search_transition_without_rearming_from_history() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let threshold = astra_turn_core::evaluation::SEARCH_FANOUT_THRESHOLD as u32;
        let mut records = (1..=threshold + 1)
            .map(|round| {
                executed(
                    "bash",
                    &format!(r#"{{"command":"rg question-{round} src"}}"#),
                    round,
                )
            })
            .collect::<Vec<_>>();
        let _ = evaluate_tool_boundary(
            &mut state,
            subject.clone(),
            &records[..threshold as usize],
            threshold,
        );
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 1);

        records.push(executed(
            "read_file",
            r#"{"path":"src/exact_gap.rs"}"#,
            threshold + 2,
        ));
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 2);
        records.push(executed(
            "bash",
            r#"{"command":"cargo test"}"#,
            threshold + 3,
        ));
        let acted = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 3)
            .expect("decisive action updates feedback");
        assert!(!feedback_requires_convergence(&acted));

        for offset in 4..=5 {
            records.push(executed(
                "read_file",
                &format!(r#"{{"path":"src/post-action-{offset}.rs"}}"#),
                threshold + offset,
            ));
            let feedback =
                evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + offset)
                    .expect("post-action inspection is evaluated");
            assert!(
                !feedback_requires_convergence(&feedback),
                "historical fan-out must not immediately re-arm ignored-advisory state"
            );
        }
    }

    #[test]
    fn introspect_does_not_masquerade_as_task_action_after_search_advisory() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let threshold = astra_turn_core::evaluation::SEARCH_FANOUT_THRESHOLD as u32;
        let mut records = (1..=threshold + 1)
            .map(|round| {
                executed(
                    "bash",
                    &format!(r#"{{"command":"rg question-{round} src"}}"#),
                    round,
                )
            })
            .collect::<Vec<_>>();
        let _ = evaluate_tool_boundary(
            &mut state,
            subject.clone(),
            &records[..threshold as usize],
            threshold,
        );
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 1);

        records.push(executed(
            "introspect",
            r#"{"facet":"overview","depth":"diagnostic","horizon":"recent"}"#,
            threshold + 2,
        ));
        let first = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 2)
            .expect("introspection boundary is evaluated");
        assert!(!feedback_requires_convergence(&first));

        records.push(executed(
            "read_file",
            r#"{"path":"src/still-inspecting.rs"}"#,
            threshold + 3,
        ));
        let ignored = evaluate_tool_boundary(&mut state, subject, &records, threshold + 3)
            .expect("continued inspection is evaluated");
        assert!(feedback_requires_convergence(&ignored));
    }

    #[test]
    fn batched_early_fanout_can_surface_ignored_guidance_before_round_churn_gate() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let threshold = astra_turn_core::evaluation::SEARCH_FANOUT_THRESHOLD as u32;
        let mut records = (0..threshold)
            .map(|index| {
                executed(
                    "bash",
                    &format!(r#"{{"command":"rg batch-a-{index} src"}}"#),
                    1,
                )
            })
            .collect::<Vec<_>>();
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, 1);
        records.push(executed("bash", r#"{"command":"rg batch-b src"}"#, 2));
        let converged = evaluate_tool_boundary(&mut state, subject.clone(), &records, 2)
            .expect("second search boundary converges");
        assert!(!feedback_requires_convergence(&converged));

        records.push(executed("list_dir", r#"{"path":"src"}"#, 3));
        let grace = evaluate_tool_boundary(&mut state, subject.clone(), &records, 3);
        assert!(
            grace
                .as_ref()
                .is_none_or(|feedback| !feedback_requires_convergence(feedback)),
            "one directory inspection remains allowed"
        );
        records.push(executed("git", r#"{"action":"show","ref":"HEAD"}"#, 4));
        let ignored = evaluate_tool_boundary(&mut state, subject, &records, 4)
            .expect("second observation advances guidance");
        assert!(feedback_requires_convergence(&ignored));
    }

    #[test]
    fn non_authoritative_lifecycle_record_preserves_active_watch_and_revision() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let threshold = astra_turn_core::evaluation::SEARCH_FANOUT_THRESHOLD as u32;
        let mut records = (1..=threshold + 1)
            .map(|round| {
                executed(
                    "bash",
                    &format!(r#"{{"command":"rg lifecycle-{round} src"}}"#),
                    round,
                )
            })
            .collect::<Vec<_>>();
        let _ = evaluate_tool_boundary(
            &mut state,
            subject.clone(),
            &records[..threshold as usize],
            threshold,
        );
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 1);
        records.push(executed(
            "read_file",
            r#"{"path":"src/grace.rs"}"#,
            threshold + 2,
        ));
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 2);
        let before = state.latest().clone();

        records.push(ToolCallRecord {
            name: "settle_work_item".to_string(),
            round: Some(threshold + 3),
            disposition: Some(ToolCallDisposition::Reused),
            ok: true,
            ..Default::default()
        });
        assert!(
            evaluate_tool_boundary(&mut state, subject.clone(), &records, threshold + 3).is_none(),
            "reused lifecycle metadata must not create a semantic revision"
        );
        assert_eq!(state.latest(), &before);

        records.push(executed(
            "read_file",
            r#"{"path":"src/still-observing.rs"}"#,
            threshold + 4,
        ));
        let ignored = evaluate_tool_boundary(&mut state, subject, &records, threshold + 4)
            .expect("next authoritative inspection consumes the active watch");
        assert!(feedback_requires_convergence(&ignored));
    }

    #[test]
    fn typed_tool_categories_separate_common_observations_from_actions() {
        for record in [
            executed("list_dir", r#"{"path":"src"}"#, 1),
            executed("git", r#"{"action":"status"}"#, 1),
            executed("git", r#"{"action":"log","max_count":5}"#, 1),
            executed("bash", r#"{"command":"head -n 20 src/lib.rs"}"#, 1),
            executed("introspect", r#"{"facet":"overview"}"#, 1),
        ] {
            assert!(record_is_observation_only(&record), "record: {record:?}");
            assert!(
                !record_is_decisive_transition(&record),
                "record: {record:?}"
            );
        }
        for record in [
            executed("bash", r#"{"command":"cargo test"}"#, 1),
            typed_writer("src/fix.rs", 1),
        ] {
            assert!(record_is_decisive_transition(&record), "record: {record:?}");
        }
        for record in [
            executed("write_file", r#"{"path":"src/fix.rs","content":"x"}"#, 1),
            executed("bash", r#"{"command":"pip install package"}"#, 1),
            failed(
                "bash",
                r#"{"command":"scratch-compile"}"#,
                1,
                "execution_error",
            ),
        ] {
            assert!(
                !record_is_decisive_transition(&record),
                "non-task-facing shell activity must not reset convergence: {record:?}"
            );
        }
        let rejected_reader = ToolCallRecord {
            name: "read_file".to_string(),
            args_full: Some(r#"{"path":"src/missing.rs"}"#.to_string()),
            disposition: Some(ToolCallDisposition::Rejected),
            ok: false,
            ..Default::default()
        };
        assert!(record_is_observation_only(&rejected_reader));
        assert!(!record_is_decisive_transition(&rejected_reader));

        let mixed = [
            executed("read_file", r#"{"path":"src/a.rs"}"#, 1),
            executed("write_file", r#"{"path":"src/a.rs","content":"x"}"#, 1),
        ];
        assert!(
            !mixed.iter().any(record_is_decisive_transition),
            "an unreceipted write is not task-facing policy progress"
        );
        assert!(!mixed.iter().all(record_is_observation_only));
    }

    #[test]
    fn trusted_changed_receipt_clears_active_search_watch() {
        let mut state = RuntimePolicyEvaluationState {
            search_converged_followup_inspections: Some(1),
            ..Default::default()
        };
        let record = ToolCallRecord {
            name: "bash".to_string(),
            args_full: Some(r#"{"command":"head -n 20 src/lib.rs"}"#.to_string()),
            disposition: Some(ToolCallDisposition::Executed),
            ok: true,
            round: Some(1),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.to_string(),
            ),
            workspace_mutation_receipt: Some(
                astra_tools::workspace_observation::changed_receipt()
                    .remove(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .expect("changed receipt"),
            ),
            ..Default::default()
        };

        assert!(!record_is_observation_only(&record));
        assert!(record_is_decisive_transition(&record));
        let feedback = evaluate_tool_boundary(&mut state, RuntimePolicySubject::Run, &[record], 1);
        assert!(state.search_converged_followup_inspections.is_none());
        assert!(
            feedback
                .as_ref()
                .is_none_or(|set| !feedback_requires_convergence(set))
        );
    }

    #[test]
    fn untrusted_changed_receipt_cannot_clear_active_search_watch() {
        for (scope, receipt) in [
            (
                "external",
                astra_tools::workspace_observation::changed_receipt()
                    .remove(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .expect("changed receipt"),
            ),
            (
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE,
                serde_json::json!({"schema":"forged", "changed":true}),
            ),
        ] {
            let mut state = RuntimePolicyEvaluationState {
                search_converged_followup_inspections: Some(1),
                ..Default::default()
            };
            let record = ToolCallRecord {
                name: "bash".to_string(),
                args_full: Some(r#"{"command":"head -n 20 src/lib.rs"}"#.to_string()),
                disposition: Some(ToolCallDisposition::Executed),
                ok: true,
                round: Some(1),
                workspace_mutation_observed: Some(true),
                workspace_mutation_scope: Some(scope.to_string()),
                workspace_mutation_receipt: Some(receipt),
                ..Default::default()
            };

            assert!(record_is_observation_only(&record));
            assert!(!record_is_decisive_transition(&record));
            let feedback =
                evaluate_tool_boundary(&mut state, RuntimePolicySubject::Run, &[record], 1)
                    .expect("second inspection advances ignored-advisory guidance");
            assert!(feedback_requires_convergence(&feedback));
            assert_eq!(state.search_converged_followup_inspections, Some(2));
        }
    }

    #[test]
    fn successful_lifecycle_transition_clears_active_search_watch() {
        for disposition in [ToolCallDisposition::Executed, ToolCallDisposition::Rejected] {
            let mut state = RuntimePolicyEvaluationState {
                search_converged_followup_inspections: Some(1),
                ..Default::default()
            };
            let records = vec![ToolCallRecord {
                name: "settle_work_item".to_string(),
                round: Some(1),
                disposition: Some(disposition),
                ok: disposition == ToolCallDisposition::Executed,
                ..Default::default()
            }];
            let _ = evaluate_tool_boundary(&mut state, RuntimePolicySubject::Run, &records, 1);
            assert_eq!(
                state.search_converged_followup_inspections.is_none(),
                disposition == ToolCallDisposition::Executed,
                "only a successful lifecycle disposition may clear the watch: {disposition:?}"
            );
        }
    }

    #[test]
    fn unprojected_search_convergence_cannot_arm_ignored_advisory_watch() {
        let thresholds = astra_turn_core::evaluation::EvaluationThresholds {
            redundant_overlapping_reads: 1,
            search_fanout: 1,
            redundant_validation_retries: usize::MAX,
            llm_round_churn: usize::MAX,
            exploration_family_churn: 1,
        };
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = vec![
            executed("git_diff", r#"{"path":"src/a.rs"}"#, 1),
            executed("git_diff", r#"{"path":"src/b.rs"}"#, 1),
            executed("bash", r#"{"command":"rg first src"}"#, 2),
        ];
        let _ = evaluate_tool_boundary_with_thresholds(
            &mut state,
            subject.clone(),
            &records,
            2,
            thresholds,
        );
        records.extend([
            executed("read_file", r#"{"path":"src/a.rs"}"#, 3),
            executed("read_file", r#"{"path":"src/a.rs"}"#, 3),
            failed(
                "read_file",
                r#"{"path":"src/missing.rs"}"#,
                3,
                "execution_error",
            ),
            ToolCallRecord {
                name: "read_file".to_string(),
                args_full: Some(r#"{"path":"src/blocked.rs"}"#.to_string()),
                round: Some(3),
                disposition: Some(ToolCallDisposition::Rejected),
                ok: false,
                result_class: Some("rejected".to_string()),
                ..Default::default()
            },
            executed("bash", r#"{"command":"rg second src"}"#, 3),
        ]);
        let saturated = evaluate_tool_boundary_with_thresholds(
            &mut state,
            subject.clone(),
            &records,
            3,
            thresholds,
        )
        .expect("saturated projection is evaluated");
        assert!(
            entries(&saturated)
                .iter()
                .all(|entry| entry.signal != RuntimePolicySignal::SearchFanout),
            "the bounded projection intentionally omitted SearchFanout: {saturated:?}"
        );

        for round in 4..=5 {
            records.push(executed(
                "read_file",
                &format!(r#"{{"path":"src/after-{round}.rs"}}"#),
                round,
            ));
            let feedback = evaluate_tool_boundary_with_thresholds(
                &mut state,
                subject.clone(),
                &records,
                round,
                thresholds,
            );
            let current = feedback.as_ref().unwrap_or_else(|| state.latest());
            assert!(
                !feedback_requires_convergence(current),
                "an advisory the model never received cannot be classified as ignored"
            );
        }
    }

    #[test]
    fn delivered_search_watch_survives_later_projection_eviction() {
        let thresholds = astra_turn_core::evaluation::EvaluationThresholds {
            redundant_overlapping_reads: 1,
            search_fanout: 1,
            redundant_validation_retries: usize::MAX,
            llm_round_churn: usize::MAX,
            exploration_family_churn: 1,
        };
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = vec![executed("bash", r#"{"command":"rg first src"}"#, 1)];
        let _ = evaluate_tool_boundary_with_thresholds(
            &mut state,
            subject.clone(),
            &records,
            1,
            thresholds,
        );
        records.push(executed("bash", r#"{"command":"rg second src"}"#, 2));
        let delivered = evaluate_tool_boundary_with_thresholds(
            &mut state,
            subject.clone(),
            &records,
            2,
            thresholds,
        )
        .expect("search convergence is projected");
        assert!(entries(&delivered).iter().any(|entry| {
            entry.signal == RuntimePolicySignal::SearchFanout
                && entry.stage == RuntimePolicyStage::Converge
        }));

        records.extend([
            executed("read_file", r#"{"path":"src/a.rs"}"#, 3),
            executed("read_file", r#"{"path":"src/a.rs"}"#, 3),
            failed(
                "read_file",
                r#"{"path":"src/missing.rs"}"#,
                3,
                "execution_error",
            ),
            ToolCallRecord {
                name: "read_file".to_string(),
                args_full: Some(r#"{"path":"src/blocked.rs"}"#.to_string()),
                round: Some(3),
                disposition: Some(ToolCallDisposition::Rejected),
                ok: false,
                result_class: Some("rejected".to_string()),
                ..Default::default()
            },
        ]);
        let evicted = evaluate_tool_boundary_with_thresholds(
            &mut state,
            subject.clone(),
            &records,
            3,
            thresholds,
        )
        .expect("the saturated projection changes");
        assert!(
            entries(&evicted)
                .iter()
                .all(|entry| entry.signal != RuntimePolicySignal::SearchFanout)
        );

        records.push(executed(
            "read_file",
            r#"{"path":"src/second-followup.rs"}"#,
            4,
        ));
        let ignored =
            evaluate_tool_boundary_with_thresholds(&mut state, subject, &records, 4, thresholds)
                .expect("ignored guidance reserves a projection slot");
        assert!(feedback_requires_convergence(&ignored));
    }

    #[test]
    fn fresh_search_cycle_rearms_after_sliding_window_and_decisive_action() {
        let thresholds = astra_turn_core::evaluation::EvaluationThresholds {
            search_fanout: 2,
            llm_round_churn: usize::MAX,
            ..Default::default()
        };
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = Vec::new();
        for round in 1..=3 {
            records.push(executed(
                "bash",
                &format!(r#"{{"command":"rg old-{round} src"}}"#),
                round,
            ));
            let _ = evaluate_tool_boundary_with_thresholds(
                &mut state,
                subject.clone(),
                &records,
                round,
                thresholds,
            );
        }
        records.push(executed("bash", r#"{"command":"cargo test"}"#, 4));
        let reset = evaluate_tool_boundary_with_thresholds(
            &mut state,
            subject.clone(),
            &records,
            4,
            thresholds,
        );
        assert!(state.search_converged_followup_inspections.is_none());
        let current = reset.as_ref().unwrap_or_else(|| state.latest());
        assert!(!feedback_requires_convergence(current));

        for round in 5..=69 {
            records.push(executed(
                "list_dir",
                &format!(r#"{{"path":"src/dir-{round}"}}"#),
                round,
            ));
            let _ = evaluate_tool_boundary_with_thresholds(
                &mut state,
                subject.clone(),
                &records,
                round,
                thresholds,
            );
        }
        for round in 70..=72 {
            records.push(executed(
                "bash",
                &format!(r#"{{"command":"rg fresh-{round} src"}}"#),
                round,
            ));
            let _ = evaluate_tool_boundary_with_thresholds(
                &mut state,
                subject.clone(),
                &records,
                round,
                thresholds,
            );
        }
        for round in 73..=74 {
            records.push(executed(
                "read_file",
                &format!(r#"{{"path":"src/fresh-followup-{round}.rs"}}"#),
                round,
            ));
            let _ = evaluate_tool_boundary_with_thresholds(
                &mut state,
                subject.clone(),
                &records,
                round,
                thresholds,
            );
        }
        assert!(
            feedback_requires_convergence(state.latest()),
            "a fresh post-action fan-out cycle must be independently trackable"
        );
    }

    #[test]
    fn failed_or_rejected_batch_cannot_clear_sticky_convergence() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = (1..=10)
            .map(|round| failed("probe", "{}", round, "execution_error"))
            .collect::<Vec<_>>();
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records[..8], 8);
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records[..9], 9);
        let converged = evaluate_tool_boundary(&mut state, subject.clone(), &records, 10)
            .expect("persistent failure cadence converges");
        assert!(
            feedback_requires_convergence(&converged),
            "unexpected feedback: {converged:?}"
        );

        records.push(failed("probe-a", "{}", 11, "execution_error"));
        records.push(ToolCallRecord {
            name: "probe-b".to_string(),
            round: Some(11),
            disposition: Some(ToolCallDisposition::Rejected),
            ok: false,
            result_class: Some("rejected".to_string()),
            ..Default::default()
        });
        let still_converged = evaluate_tool_boundary(&mut state, subject, &records, 11)
            .expect("failed batch is authoritative failure evidence");
        assert!(
            feedback_requires_convergence(&still_converged),
            "more failures cannot masquerade as recovery"
        );
    }

    #[test]
    fn rotating_failure_cause_restarts_at_observe() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = vec![failed("tool-a", "{}", 1, "execution_error")];
        let first = evaluate_tool_boundary(&mut state, subject.clone(), &records, 1)
            .expect("first cause is observed");
        assert_eq!(
            entries(&first)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );

        records.push(executed("tool-a", r#"{"fix":true}"#, 2));
        records.push(failed("tool-b", "{}", 2, "execution_error"));
        let _ = evaluate_tool_boundary(&mut state, subject, &records, 2);
        let rotated = state.latest();
        assert_eq!(
            entries(rotated)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe),
            "a first failure of tool-b is not persistence of recovered tool-a"
        );
    }

    #[test]
    fn sticky_convergence_does_not_transfer_to_a_first_new_failure() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = (1..=8)
            .map(|round| executed("probe", &format!(r#"{{"step":{round}}}"#), round))
            .collect::<Vec<_>>();
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, 8);
        for round in 9..=11 {
            records.push(failed("tool-a", "{}", round, "execution_error"));
            let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, round);
        }
        assert!(feedback_requires_convergence(state.latest()));

        records.push(failed("tool-b", "{}", 12, "execution_error"));
        records.push(executed("tool-a", "{}", 13));
        let demoted = evaluate_tool_boundary(&mut state, subject, &records, 13)
            .expect("old cause recovery and new cause are evaluated together");
        assert!(
            !feedback_requires_convergence(&demoted),
            "a first failure of tool-b cannot inherit tool-a's authority"
        );
    }

    #[test]
    fn independently_persistent_new_cause_is_captured_during_existing_convergence() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = RuntimePolicySubject::Run;
        let mut records = (1..=8)
            .map(|round| executed("probe", &format!(r#"{{"step":{round}}}"#), round))
            .collect::<Vec<_>>();
        let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, 8);
        for round in 9..=11 {
            records.push(failed("tool-a", "{}", round, "execution_error"));
            let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, round);
        }
        assert!(feedback_requires_convergence(state.latest()));

        for round in 12..=14 {
            records.push(failed("tool-b", "{}", round, "execution_error"));
            let _ = evaluate_tool_boundary(&mut state, subject.clone(), &records, round);
        }
        records.push(executed("tool-a", "{}", 15));
        let b_remains = evaluate_tool_boundary(&mut state, subject.clone(), &records, 15)
            .expect("tool-a recovery changes the captured cause set");
        assert!(
            feedback_requires_convergence(&b_remains),
            "tool-b independently persisted and still needs recovery"
        );

        records.push(executed("tool-b", "{}", 16));
        let recovered = evaluate_tool_boundary(&mut state, subject, &records, 16)
            .expect("all independently captured causes recovered");
        assert!(!feedback_requires_convergence(&recovered));
    }

    #[test]
    fn converged_round_churn_does_not_oscillate_when_window_ages_out_corroboration() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let mut records = vec![failed(
            "read_file",
            r#"{"path":"missing"}"#,
            1,
            "execution_error",
        )];
        records.extend((2..=7).map(|round| {
            executed(
                "introspect",
                &format!(r#"{{"facet":"round-{round}"}}"#),
                round,
            )
        }));

        let observed = evaluate_tool_boundary(&mut state, subject.clone(), &records, 8)
            .expect("the unresolved outcome remains visible");
        assert_eq!(
            entries(&observed)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            None,
            "one never-reobserved recoverable failure is not low-yield corroboration"
        );

        records.push(failed(
            "read_file",
            r#"{"path":"missing"}"#,
            8,
            "execution_error",
        ));
        let grace = evaluate_tool_boundary(&mut state, subject.clone(), &records, 9)
            .expect("the independent cause converges before scheduler pressure");
        assert_eq!(
            entries(&grace)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );

        records.push(failed(
            "read_file",
            r#"{"path":"missing"}"#,
            9,
            "execution_error",
        ));
        let converged = evaluate_tool_boundary(&mut state, subject.clone(), &records, 10)
            .expect("persistent correlated churn converges after the grace boundary");
        assert_eq!(
            entries(&converged)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Converge)
        );

        // Push the original failure outside the bounded detector window.
        // Aging is not authoritative recovery, so the cause stays sticky.
        records.extend((10..=65).map(|round| {
            executed(
                "introspect",
                &format!(r#"{{"facet":"aging-{round}"}}"#),
                round,
            )
        }));
        let still_converged = evaluate_tool_boundary(&mut state, subject.clone(), &records, 65)
            .expect("window aging changes the visible projection");
        assert_eq!(
            entries(&still_converged)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Converge),
            "window aging must not masquerade as outcome recovery"
        );

        records.push(executed("read_file", r#"{"path":"missing"}"#, 66));
        let recovered = evaluate_tool_boundary(&mut state, subject, &records, 66)
            .expect("same-tool success resolves the typed cause");
        assert_eq!(
            entries(&recovered)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::LowYieldRoundChurn)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe),
            "authoritative same-tool recovery must demote scheduler pressure"
        );
        assert!(!feedback_requires_convergence(&recovered));
    }

    #[test]
    fn subject_switch_keeps_run_failures_but_drops_prior_item_exploration() {
        let mut state = RuntimePolicyEvaluationState::default();
        let mut records = vec![
            failed("bash", r#"{"command":"cargo test"}"#, 1, "env_failure"),
            failed("bash", r#"{"command":"cargo test"}"#, 2, "execution_error"),
        ];
        for round in 3..=5 {
            records.push(executed(
                "grep",
                &format!(r#"{{"pattern":"p-{round}"}}"#),
                round,
            ));
            records.push(executed(
                "glob",
                &format!(r#"{{"pattern":"file-{round}-*.rs"}}"#),
                round,
            ));
        }
        let first = evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 5)
            .expect("first subject evidence");
        assert!(
            entries(&first)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::ExplorationFamilyChurn)
        );

        let switched = evaluate_tool_boundary(&mut state, work_subject("item-2"), &records, 5)
            .expect("subject identity changes the projection");
        assert!(
            entries(&switched)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
        );
        assert!(
            !entries(&switched)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::ExplorationFamilyChurn)
        );
    }

    #[test]
    fn persistent_failures_escalate_to_typed_introspect_guidance() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let mut records = vec![
            failed("bash", r#"{"command":"cargo test"}"#, 1, "env_failure"),
            failed("bash", r#"{"command":"cargo test"}"#, 2, "execution_error"),
        ];
        let first = evaluate_tool_boundary(&mut state, subject.clone(), &records, 2)
            .expect("first typed diagnosis");
        assert!(!feedback_requires_outcome_reconciliation(&first));
        assert_eq!(
            entries(&first)
                .iter()
                .find(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
                .map(|entry| entry.stage),
            Some(RuntimePolicyStage::Observe)
        );

        records.push(failed(
            "bash",
            r#"{"command":"cargo test"}"#,
            3,
            "test_failure",
        ));
        let converged = evaluate_tool_boundary(&mut state, subject, &records, 3)
            .expect("persistent failure must advance its policy stage");
        let payload = policy_advisory_payload(&converged).expect("model advisory");
        assert!(feedback_requires_outcome_reconciliation(&converged));
        let entry = payload["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["signal"] == "unresolved_tool_outcomes")
            .expect("unresolved outcome advisory");
        assert_eq!(entry["stage"], "converge");
        assert_eq!(entry["recommendation"], "diagnose_tool_outcomes");
        assert!(
            entry["instruction"]
                .as_str()
                .unwrap()
                .contains("introspect")
        );
    }

    #[test]
    fn observe_feedback_is_actionable_without_becoming_execution_control() {
        let synthesis = recommendation_text(
            RuntimePolicyRecommendation::SynthesizeAndDecide,
            RuntimePolicyStage::Observe,
        );
        assert!(synthesis.contains("leading hypothesis"));
        assert!(synthesis.contains("one falsifier"));
        assert!(synthesis.contains("still-unmet user predicate"));
        assert!(synthesis.contains("complete unmodified project acceptance harness"));
        assert!(synthesis.contains("authorized change"));
        assert!(synthesis.contains("Do not repeat equivalent probes"));
        assert!(synthesis.contains("narrate the plan"));
        assert!(synthesis.len() <= 500, "dynamic advisory must stay compact");

        let converge = recommendation_text(
            RuntimePolicyRecommendation::SynthesizeAndDecide,
            RuntimePolicyStage::Converge,
        );
        assert!(converge.contains("Stop new exploration"));
        assert!(converge.contains("stop restating the plan"));
        assert!(converge.contains("remaining authorized mutation"));
        assert!(converge.contains("after the final mutation"));
        assert!(converge.contains("complete unmodified acceptance harness"));
        assert!(converge.contains("exact unresolved boundary"));
        assert!(converge.contains("directly close a named user predicate"));
        assert!(converge.len() <= 500, "dynamic advisory must stay compact");

        let outcomes = recommendation_text(
            RuntimePolicyRecommendation::DiagnoseToolOutcomes,
            RuntimePolicyStage::Observe,
        );
        assert!(outcomes.contains("known-good alternative or introspect"));
        assert!(outcomes.contains("do not settle it as delivered"));
        assert!(outcomes.contains("settle blocked/failed"));
    }

    #[test]
    fn successful_capability_recovery_clears_online_failure_advisory() {
        let mut state = RuntimePolicyEvaluationState::default();
        let subject = work_subject("item-1");
        let mut records = vec![
            failed("bash", r#"{"command":"cargo test"}"#, 1, "test_failure"),
            failed("bash", r#"{"command":"cargo test"}"#, 2, "test_failure"),
        ];
        let failing = evaluate_tool_boundary(&mut state, subject.clone(), &records, 2)
            .expect("failures produce online guidance");
        assert!(
            entries(&failing)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
        );

        records.push(executed("bash", r#"{"command":"cargo test"}"#, 3));
        let recovered = evaluate_tool_boundary(&mut state, subject.clone(), &records, 3)
            .expect("successful capability recovery changes the projection");
        assert!(
            entries(&recovered)
                .iter()
                .all(|entry| entry.signal != RuntimePolicySignal::UnresolvedToolOutcomes),
            "historical failed attempts must not remain a present advisory: {recovered:?}"
        );

        records.push(failed(
            "bash",
            r#"{"command":"later regression check"}"#,
            4,
            "test_failure",
        ));
        let regressed = evaluate_tool_boundary(&mut state, subject, &records, 4)
            .expect("a new failure after recovery changes the projection again");
        assert!(
            entries(&regressed)
                .iter()
                .any(|entry| entry.signal == RuntimePolicySignal::UnresolvedToolOutcomes)
        );
    }

    #[test]
    fn unrelated_tool_success_does_not_clear_online_failure_advisory() {
        let records = vec![
            failed("bash", r#"{"command":"cargo test"}"#, 1, "test_failure"),
            executed("read_file", r#"{"path":"src/lib.rs"}"#, 2),
        ];
        assert_eq!(
            astra_turn_core::evaluation::count_unresolved_tool_outcome_failures(&records),
            1
        );
    }

    #[test]
    fn work_subject_transition_resolves_prior_evidence_without_reassigning_it() {
        let mut state = RuntimePolicyEvaluationState::default();
        let records = vec![
            executed("grep", r#"{"pattern":"a"}"#, 1),
            executed("grep", r#"{"pattern":"b"}"#, 2),
            executed("read_file", r#"{"path":"a.rs"}"#, 3),
        ];
        evaluate_tool_boundary(&mut state, work_subject("item-1"), &records, 3)
            .expect("first subject");
        let switched = evaluate_tool_boundary(&mut state, work_subject("item-2"), &records, 3)
            .expect("subject switch is a semantic revision");
        let RuntimePolicyFeedbackSet::Evaluated {
            subject, entries, ..
        } = switched
        else {
            panic!("evaluated feedback expected");
        };
        assert_eq!(subject, work_subject("item-2"));
        assert!(
            entries.is_empty(),
            "old evidence cannot move to the successor"
        );
    }

    #[test]
    fn policy_advisory_serializes_subject_once_and_never_as_execution_control() {
        let set = RuntimePolicyFeedbackSet::Evaluated {
            schema_version: RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            revision: 7,
            evaluated_at_round: 9,
            subject: work_subject("item-7"),
            entries: vec![RuntimePolicyFeedbackEntry {
                signal: RuntimePolicySignal::RedundantReads,
                stage: RuntimePolicyStage::Converge,
                observed_at_round: 9,
                evidence_count: 12,
                recommendation: RuntimePolicyRecommendation::ReuseKnownContent,
            }],
        };

        let payload = policy_advisory_payload(&set).expect("active feedback");
        let encoded = serde_json::to_string(&payload).expect("serialize advisory");
        assert_eq!(encoded.matches("\"subject\"").count(), 1);
        assert_eq!(payload["authority"], "advisory_evidence_only");
        for forbidden in ["max_turns", "hard_limit", "allowed_tools", "settle"] {
            assert!(
                !payload.get(forbidden).is_some(),
                "forbidden control: {forbidden}"
            );
        }
    }

    /// Build a minimal `JournalFacts` with only the legacy fields set.
    /// New fields (token_pressure, etc.) default to 0.0.
    fn facts(
        outcome_streak: u32,
        zero_streak: u32,
        budget_remaining: u32,
        budget_max: u32,
    ) -> JournalFacts {
        JournalFacts {
            budget: BudgetSnapshot {
                rounds_completed: 5,
                budget_remaining,
                budget_max,
            },
            streaks: StreakSnapshot {
                consecutive_rounds_with_outcome: outcome_streak,
                consecutive_rounds_without_outcome: zero_streak,
                consecutive_read_only: 0,
            },
            performance: PerformanceSnapshot {
                total_observation_calls: 0,
                total_errors: 0,
                total_tool_calls: 0,
                current_error_rate: 0.0,
                cache_hit_ratio: 0.0,
                token_pressure: 0.0,
            },
            stall: StallSnapshot { stall_reason: None },
        }
    }

    // ── Expansion threshold (existing) ──────────────────────────────────────

    #[test]
    fn expands_on_consecutive_outcomes() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 4, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn skips_expand_when_budget_plentiful() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 8, 10);
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn expands_exactly_at_half_budget() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 5, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn expands_before_half_budget_when_progress_is_clear() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 6, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    // ── Zero-streak signal (existing) ───────────────────────────────────────

    #[test]
    fn injects_signal_on_zero_outcomes() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 3, 7, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { .. }))
        );
    }

    #[test]
    fn no_signal_below_zero_threshold() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 2, 7, 10);
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { .. }))
        );
    }

    // ── Continue default (existing) ─────────────────────────────────────────

    #[test]
    fn continues_when_nothing_triggers() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 8, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    #[test]
    fn zero_rounds_safe_defaults() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 10, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    // ── Custom parameters (existing) ────────────────────────────────────────

    #[test]
    fn custom_params_respected() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 4,
            expand_factor: 2.0,
            max_ceiling: 200,
            reflect_after_consecutive_zero: 5,
            context_pressure: ContextPressurePolicy::default(),
            circuit: CircuitPolicy::default(),
            mark_truncated_text: true,
        };
        let f = facts(4, 0, 3, 10);
        let actions = policy.decide(&f);
        assert!(matches!(
            actions[0],
            RuntimePolicyEvidence::BudgetExpansionSuggested {
                factor: 2.0,
                max_ceiling: 200,
            }
        ));
    }

    #[test]
    fn max_ceiling_respected_by_caller() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 1,
            expand_factor: 100.0,
            max_ceiling: 100,
            reflect_after_consecutive_zero: 3,
            context_pressure: ContextPressurePolicy::default(),
            circuit: CircuitPolicy::default(),
            mark_truncated_text: true,
        };
        let f = facts(1, 0, 5, 10);
        let actions = policy.decide(&f);
        assert!(matches!(
            actions[0],
            RuntimePolicyEvidence::BudgetExpansionSuggested {
                max_ceiling: 100,
                ..
            }
        ));
    }

    // ── Stall override (existing) ───────────────────────────────────────────

    #[test]
    fn stall_overrides_all() {
        let policy = RuntimePolicy::default();
        let f = JournalFacts {
            stall: StallSnapshot {
                stall_reason: Some("Same tools called 3 times in a row".into()),
            },
            streaks: StreakSnapshot {
                consecutive_rounds_with_outcome: 3,
                consecutive_rounds_without_outcome: 3,
                consecutive_read_only: 0,
            },
            budget: BudgetSnapshot {
                rounds_completed: 5,
                budget_remaining: 2,
                budget_max: 10,
            },
            performance: PerformanceSnapshot {
                total_observation_calls: 0,
                total_errors: 0,
                total_tool_calls: 0,
                current_error_rate: 0.5,
                cache_hit_ratio: 0.0,
                token_pressure: 0.95,
            },
        };
        let actions = policy.decide(&f);
        // Stall signal takes priority; only one action returned even though
        // token pressure and error rate both suggest other actions.
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            RuntimePolicyEvidence::Advisory { .. }
        ));
    }

    // ── E2E: full pipeline (existing) ───────────────────────────────────────

    #[test]
    fn e2e_outcome_streak_expands_budget() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 3, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn e2e_zero_streak_injects_signal() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 3, 7, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { .. }))
        );
    }

    #[test]
    fn e2e_normal_state_returns_continue() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 8, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    #[test]
    fn e2e_full_pipeline_all_paths_exercised() {
        let policy = RuntimePolicy::default();

        // State 1: normal → Continue
        let actions = policy.decide(&facts(0, 0, 8, 10));
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));

        // State 2: outcome streak + tight budget → BudgetExpansionSuggested
        let actions = policy.decide(&facts(2, 0, 3, 10));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );

        // State 3: zero streak → Advisory
        let actions = policy.decide(&facts(0, 3, 7, 10));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { .. }))
        );

        // State 4: stall → Advisory only
        let mut stall_facts = facts(2, 0, 3, 10);
        stall_facts.stall.stall_reason = Some("stall".into());
        let actions = policy.decide(&stall_facts);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], RuntimePolicyEvidence::Advisory { .. }));

        // State 5: normal → Continue
        let actions = policy.decide(&facts(1, 1, 8, 10));
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // NEW: Compaction, Circuit Breaker, Phase Transition tests
    // ══════════════════════════════════════════════════════════════════════════

    // ── Context pressure guidance ─────────────────────────────────────────

    #[test]
    fn context_pressure_signal_normal_on_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.performance.token_pressure = 0.75;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            }
        )));
    }

    #[test]
    fn context_pressure_signal_aggressive_on_high_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.performance.token_pressure = 0.95;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn context_pressure_signal_not_triggered_low_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.performance.token_pressure = 0.60;
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::ContextPressureObserved { .. }))
        );
    }

    #[test]
    fn context_pressure_signal_boundary_exact_threshold() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.performance.token_pressure = 0.70; // exactly at pressure_threshold
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            }
        )));
    }

    #[test]
    fn context_pressure_signal_boundary_just_below_aggressive() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.performance.token_pressure = 0.89; // below aggressive_pressure_threshold (0.90)
        let actions = policy.decide(&f);
        // Should get Normal, not Aggressive
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            }
        )));
        assert!(!actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn context_pressure_signal_custom_thresholds() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            context_pressure: ContextPressurePolicy {
                pressure_threshold: 0.50,
                aggressive_pressure_threshold: 0.80,
            },
            circuit: CircuitPolicy::default(),
            mark_truncated_text: true,
        };
        let mut f = facts(0, 0, 8, 10);
        f.performance.token_pressure = 0.55; // above custom threshold of 0.50
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            }
        )));
    }

    // ── Circuit-breaker guidance (13 tests) ────────────────────────────────

    #[test]
    fn circuit_breaker_guidance_on_high_error_rate() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.performance.current_error_rate = 0.40;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. })),
            "diagnostic risk should not be converted into budget mutation: {actions:?}"
        );
    }

    #[test]
    fn circuit_breaker_guidance_on_read_only_streak() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.streaks.consecutive_read_only = 10; // above default max_consecutive_reads (8)
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("read-only")))
        );
        assert!(
            actions.iter().all(|a| !matches!(
                a,
                RuntimePolicyEvidence::BudgetExpansionSuggested { .. }
                    | RuntimePolicyEvidence::ContextPressureObserved { .. }
            )),
            "large read-only investigations should get guidance, not runtime budget mutation"
        );
    }

    #[test]
    fn circuit_breaker_guidance_on_zero_outcome_streak() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.streaks.consecutive_rounds_without_outcome = 6; // above default max_consecutive_errors (5)
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("without observable outcome")))
        );
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn circuit_breaker_not_triggered_normal() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.performance.current_error_rate = 0.10;
        f.streaks.consecutive_read_only = 2;
        f.streaks.consecutive_rounds_without_outcome = 1;
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("Circuit-breaker risk")))
        );
    }

    #[test]
    fn circuit_breaker_boundary_error_rate_exact() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.performance.current_error_rate = 0.30; // exactly at threshold — NOT triggered (strict >)
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("Circuit-breaker risk")))
        );
    }

    #[test]
    fn circuit_breaker_boundary_error_rate_just_above_signals() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.performance.current_error_rate = 0.31; // just above threshold — triggered
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
    }

    #[test]
    fn circuit_breaker_custom_thresholds_signal() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            context_pressure: ContextPressurePolicy::default(),
            circuit: CircuitPolicy {
                max_consecutive_errors: 3,
                max_consecutive_reads: 5,
                error_rate_threshold: 0.10,
            },
            mark_truncated_text: true,
        };
        let mut f = facts(0, 0, 8, 20);
        f.performance.current_error_rate = 0.15;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
    }

    // ── Multiple actions same turn ─────────────────────────────────────────

    #[test]
    fn multiple_actions_error_rate_and_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.performance.current_error_rate = 0.40;
        f.performance.token_pressure = 0.75;
        let actions = policy.decide(&f);
        assert!(actions.len() >= 2);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::ContextPressureObserved { .. }))
        );
    }

    // ── Zero all facts → Continue (unhappy path) ───────────────────────────

    #[test]
    fn zero_all_facts_returns_continue() {
        let policy = RuntimePolicy::default();
        // All fields at their zero/default values.
        let f = JournalFacts {
            budget: BudgetSnapshot {
                rounds_completed: 0,
                budget_remaining: 10,
                budget_max: 10,
            },
            streaks: StreakSnapshot {
                consecutive_rounds_with_outcome: 0,
                consecutive_rounds_without_outcome: 0,
                consecutive_read_only: 0,
            },
            performance: PerformanceSnapshot {
                total_observation_calls: 0,
                total_errors: 0,
                total_tool_calls: 0,
                current_error_rate: 0.0,
                cache_hit_ratio: 0.0,
                token_pressure: 0.0,
            },
            stall: StallSnapshot { stall_reason: None },
        };
        let actions = policy.decide(&f);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    // ── Extreme values (unhappy path) ──────────────────────────────────────

    #[test]
    fn full_token_pressure_triggers_aggressive() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.performance.token_pressure = 1.0;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn full_error_rate_triggers_guidance_signal() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.performance.current_error_rate = 1.0;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
    }

    #[test]
    fn circuit_guidance_does_not_compute_budget_floor() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 5);
        f.performance.current_error_rate = 0.40;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. })),
            "circuit guidance must not backdoor a computed budget floor: {actions:?}"
        );
    }

    // ���═════════════════════════════════════════════════════════════════════════
    // ══════════════════════════════════════════════════════════════════════════

    // ══════════════════════════════════════════════════════════════════════════
    // Truncation Marker — unhappy path coverage
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn truncation_marker_on_length_finish_reason() {
        let policy = RuntimePolicy::default();
        let result = policy.truncation_marker(Some("length"));
        assert!(result.is_some());
        assert!(result.unwrap().contains("truncated"));
    }

    #[test]
    fn truncation_marker_not_triggered_on_stop() {
        let policy = RuntimePolicy::default();
        assert!(policy.truncation_marker(Some("stop")).is_none());
    }

    #[test]
    fn truncation_marker_not_triggered_on_tool_calls() {
        let policy = RuntimePolicy::default();
        assert!(policy.truncation_marker(Some("tool_calls")).is_none());
    }

    #[test]
    fn truncation_marker_not_triggered_on_none() {
        let policy = RuntimePolicy::default();
        assert!(policy.truncation_marker(None).is_none());
    }

    #[test]
    fn truncation_marker_disabled_returns_none() {
        let policy = RuntimePolicy {
            mark_truncated_text: false,
            ..RuntimePolicy::default()
        };
        assert!(
            policy.truncation_marker(Some("length")).is_none(),
            "disabled policy must not emit marker"
        );
    }
}
