//! Feedback from an API response — feeds into PipelineStats for the next turn's Plan.

use serde::{Deserialize, Serialize};

pub use crate::cache_diagnostics::CacheBreakReason;
use crate::compaction_types::CompactionTier;
use crate::token_accounting::TokenAccounting;

/// Stable identity of one admitted runtime observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFeedbackIdentity {
    pub session_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub model_id: String,
    pub topology: astra_services::ModelRequestTopology,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<crate::context_assembly_trace::ModelRequestTraceIdentity>,
}

impl RuntimeFeedbackIdentity {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        [
            self.session_id.as_str(),
            self.run_id.as_str(),
            self.agent_id.as_str(),
            self.model_id.as_str(),
        ]
        .into_iter()
        .all(|value| !value.trim().is_empty())
            && self.request.as_ref().is_none_or(|request| {
                !request.request_id.trim().is_empty() && !request.request_hash.trim().is_empty()
            })
    }
}

/// Unambiguous progress counters. Session turns and provider rounds are
/// deliberately separate units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFeedbackProgress {
    pub session_turn: u32,
    pub agentic_round_index: u32,
    pub llm_rounds_completed: u32,
    pub slice_round_limit: u32,
    pub slice_rounds_remaining: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absolute_round_ceiling: Option<u32>,
}

/// Context Pipeline capacity and pressure observed for the concrete request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeContextFeedback {
    /// Content identity of the stable provider-cache prefix. Produced by the
    /// wire manifest from system and tool-schema prefixes; it never contains
    /// prompt text. `None` means the concrete request did not expose a
    /// trustworthy cache epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_identity: Option<astra_turn_types::PromptCacheIdentityV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_input_limit_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_input_tokens: Option<u64>,
    /// Ratio of estimated outgoing input to the effective input limit.
    /// Values above 1.0 are meaningful pressure evidence, not invalid data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_pressure: Option<f64>,
    pub compaction_tier: CompactionTier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePolicySignal {
    ExplorationFamilyChurn,
    RedundantReads,
    UnresolvedToolOutcomes,
    RejectedToolRequests,
    SearchFanout,
    ValidationRetryChurn,
    LowYieldRoundChurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePolicyStage {
    Observe,
    Converge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePolicyRecommendation {
    TestExactHypothesis,
    ReuseKnownContent,
    DiagnoseToolOutcomes,
    RepairToolRequest,
    NarrowEvidenceSearch,
    ChangeValidationStrategy,
    SynthesizeAndDecide,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimePolicySubject {
    Run,
    WorkItem {
        attempt_id: String,
        item_id: String,
        item_revision: i64,
        objective: String,
        expected_result: String,
    },
}

impl RuntimePolicySubject {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Run => true,
            Self::WorkItem {
                attempt_id,
                item_id,
                item_revision,
                objective,
                expected_result,
            } => {
                *item_revision > 0
                    && bounded_nonempty(attempt_id, 256)
                    && bounded_nonempty(item_id, 256)
                    && bounded_nonempty(objective, 2_000)
                    && bounded_nonempty(expected_result, 2_000)
            }
        }
    }
}

fn bounded_nonempty(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_bytes
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePolicyFeedbackEntry {
    pub signal: RuntimePolicySignal,
    pub stage: RuntimePolicyStage,
    pub observed_at_round: u32,
    pub evidence_count: u32,
    pub recommendation: RuntimePolicyRecommendation,
}

impl RuntimePolicyFeedbackEntry {
    #[must_use]
    pub fn is_valid(&self, evaluated_at_round: u32) -> bool {
        self.evidence_count > 0 && self.observed_at_round <= evaluated_at_round
    }
}

/// One immutable policy evaluation delivered to a concrete provider request.
/// Unknown and evaluated-without-advisory are deliberately different states.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimePolicyFeedbackSet {
    #[default]
    NotEvaluated,
    Evaluated {
        schema_version: u32,
        revision: u32,
        evaluated_at_round: u32,
        subject: RuntimePolicySubject,
        entries: Vec<RuntimePolicyFeedbackEntry>,
    },
}

impl RuntimePolicyFeedbackSet {
    pub const SCHEMA_VERSION: u32 = 2;
    pub const MAX_ENTRIES: usize = 4;

    #[must_use]
    pub fn is_valid(&self, llm_rounds_completed: u32) -> bool {
        match self {
            Self::NotEvaluated => true,
            Self::Evaluated {
                schema_version,
                revision,
                evaluated_at_round,
                subject,
                entries,
            } => {
                *schema_version == Self::SCHEMA_VERSION
                    && *revision > 0
                    && *evaluated_at_round <= llm_rounds_completed
                    && subject.is_valid()
                    && entries.len() <= Self::MAX_ENTRIES
                    && entries
                        .iter()
                        .all(|entry| entry.is_valid(*evaluated_at_round))
                    && entries.iter().enumerate().all(|(index, entry)| {
                        entries[..index]
                            .iter()
                            .all(|prior| prior.signal != entry.signal)
                    })
            }
        }
    }

    #[must_use]
    pub fn inline_summary(&self) -> String {
        match self {
            Self::NotEvaluated => "not_evaluated".to_string(),
            Self::Evaluated {
                subject, entries, ..
            } if entries.is_empty() => format!("{}:none", policy_subject_label(subject)),
            Self::Evaluated {
                subject, entries, ..
            } => format!(
                "{}:{}",
                policy_subject_label(subject),
                entries
                    .iter()
                    .map(|entry| format!("{:?}/{:?}", entry.signal, entry.stage))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }
}

fn policy_subject_label(subject: &RuntimePolicySubject) -> String {
    match subject {
        RuntimePolicySubject::Run => "run".to_string(),
        RuntimePolicySubject::WorkItem {
            item_id,
            item_revision,
            ..
        } => format!("work_item={item_id}@{item_revision}"),
    }
}

/// Canonical post-ingest fact frame shared by Context Pipeline feedback,
/// durable observation, introspection, and future Desktop projections.
///
/// The frame contains no prompt text, tool payload, or policy command. It is a
/// bounded O(1) description of one successfully ingested provider round.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeFeedbackFrame {
    pub schema_version: u32,
    pub identity: RuntimeFeedbackIdentity,
    pub progress: RuntimeFeedbackProgress,
    pub context: RuntimeContextFeedback,
    /// Provider usage for this concrete request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_usage: Option<TokenAccounting>,
    /// Provider usage accumulated by this run after ingesting the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_usage: Option<TokenAccounting>,
    pub was_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_break_detected: Option<CacheBreakReason>,
    pub policy_feedback: RuntimePolicyFeedbackSet,
}

impl RuntimeFeedbackFrame {
    pub const SCHEMA_VERSION: u32 = 4;

    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.schema_version == Self::SCHEMA_VERSION
            && self.identity.is_valid()
            && self.progress.slice_round_limit > 0
            && self.progress.slice_rounds_remaining <= self.progress.slice_round_limit
            && self
                .progress
                .absolute_round_ceiling
                .is_none_or(|value| value > 0)
            && self
                .context
                .token_pressure
                .is_none_or(|value| value.is_finite() && value >= 0.0)
            && self
                .context
                .model_context_window_tokens
                .is_none_or(|value| value > 0)
            && self
                .context
                .effective_input_limit_tokens
                .is_none_or(|value| value > 0)
            && self
                .context
                .prompt_cache_identity
                .as_ref()
                .is_none_or(astra_turn_types::PromptCacheIdentityV1::is_valid)
            && self
                .request_usage
                .zip(self.run_usage)
                .is_none_or(|(request, run)| {
                    run.prompt >= request.prompt
                        && run.cache_read >= request.cache_read
                        && run.cache_creation >= request.cache_creation
                        && run.completion >= request.completion
                })
            && self
                .policy_feedback
                .is_valid(self.progress.llm_rounds_completed)
    }

    #[must_use]
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        self.request_usage.map(|usage| usage.cache_hit_ratio())
    }

    pub fn detect_cache_break(&mut self, turn: u32, min_creation_threshold: u64) {
        if turn > 1
            && self.request_usage.is_some_and(|usage| {
                usage.cache_read == 0 && usage.cache_creation >= min_creation_threshold
            })
            && self.cache_break_detected.is_none()
        {
            self.cache_break_detected = Some(CacheBreakReason::UnknownColdStart);
        }
    }

    pub fn attribute_cache_break(&mut self, reason: CacheBreakReason) {
        self.cache_break_detected = Some(reason);
    }

    /// Whether the authoritative runtime still carries a *converged*
    /// unresolved tool fact at this boundary.
    ///
    /// `Observe` is deliberately not terminal evidence. A single failed
    /// probe or rejected request is often an expected branch of an
    /// investigation; it should reach the model as an alert and leave the
    /// model free to explain the result or continue. Only a signal that has
    /// crossed the policy hysteresis boundary (`Converge`) is strong enough
    /// to prevent a server-owned response from claiming verified completion.
    /// This keeps feedback advisory without making transient failures a hard
    /// stop.
    #[must_use]
    pub fn has_unresolved_tool_outcomes(&self) -> bool {
        match &self.policy_feedback {
            RuntimePolicyFeedbackSet::Evaluated { entries, .. } => entries.iter().any(|entry| {
                entry.stage == RuntimePolicyStage::Converge
                    && matches!(
                        entry.signal,
                        RuntimePolicySignal::UnresolvedToolOutcomes
                            | RuntimePolicySignal::RejectedToolRequests
                    )
            }),
            RuntimePolicyFeedbackSet::NotEvaluated => false,
        }
    }
}

/// Feedback from a single API response. Produced by Execute, consumed by
/// PipelineStats::record().
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextFeedback {
    pub tokens: TokenAccounting,
    pub cache_hit_ratio: f64,
    pub was_truncated: bool,
    pub cache_break_detected: Option<CacheBreakReason>,
}

impl ContextFeedback {
    /// Build feedback from raw token usage fields.
    ///
    /// Cache read share = cache_read / total input tokens.
    /// Returns 0.0 when total input is zero (not NaN).
    #[must_use]
    pub fn from_usage(
        prompt: u64,
        cache_read: u64,
        cache_creation: u64,
        completion: u64,
        was_truncated: bool,
    ) -> Self {
        let tokens = TokenAccounting::from_fields(prompt, cache_read, cache_creation, completion);
        let cache_hit_ratio = tokens.cache_hit_ratio();
        Self {
            tokens,
            cache_hit_ratio,
            was_truncated,
            cache_break_detected: None,
        }
    }

    /// Detect a cache break from cold creation (no cache reads, significant creation).
    /// Call this with the turn number to determine if a break occurred.
    pub fn detect_cache_break(&mut self, turn: u32, min_creation_threshold: u64) {
        if turn > 1
            && self.tokens.cache_read == 0
            && self.tokens.cache_creation >= min_creation_threshold
        {
            if self.cache_break_detected.is_none() {
                self.cache_break_detected = Some(CacheBreakReason::UnknownColdStart);
            }
        }
    }

    /// Explicitly attribute a cache break reason (replaces UnknownColdStart if set).
    pub fn attribute_cache_break(&mut self, reason: CacheBreakReason) {
        self.cache_break_detected = Some(reason);
    }

    /// No-op feedback (for EXPLAIN-only mode where no API call was made).
    #[must_use]
    pub fn none() -> Self {
        Self {
            tokens: TokenAccounting::default(),
            cache_hit_ratio: 0.0,
            was_truncated: false,
            cache_break_detected: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_usage_computes_ratio() {
        let f = ContextFeedback::from_usage(100, 800, 100, 100, false);
        assert!((f.cache_hit_ratio - 0.8).abs() < 1e-9);
    }

    #[test]
    fn zero_cache_returns_zero_ratio_not_nan() {
        let f = ContextFeedback::from_usage(1000, 0, 0, 500, false);
        assert_eq!(f.cache_hit_ratio, 0.0);
        assert!(!f.cache_hit_ratio.is_nan());
    }

    #[test]
    fn detects_cache_break_from_cold_creation() {
        let mut f = ContextFeedback::from_usage(0, 0, 5000, 100, false);
        f.detect_cache_break(2, 1000);
        assert_eq!(
            f.cache_break_detected,
            Some(CacheBreakReason::UnknownColdStart)
        );
    }

    #[test]
    fn no_cache_break_on_turn_1() {
        let mut f = ContextFeedback::from_usage(0, 0, 5000, 100, false);
        f.detect_cache_break(1, 1000);
        assert!(f.cache_break_detected.is_none());
    }

    #[test]
    fn attribute_replaces_unknown() {
        let mut f = ContextFeedback::from_usage(0, 0, 5000, 100, false);
        f.detect_cache_break(2, 1000);
        f.attribute_cache_break(CacheBreakReason::ToolSchemasChanged {
            added: vec!["bash".to_string()],
            removed: Vec::new(),
            changed: Vec::new(),
        });
        assert_eq!(
            f.cache_break_detected,
            Some(CacheBreakReason::ToolSchemasChanged {
                added: vec!["bash".to_string()],
                removed: Vec::new(),
                changed: Vec::new(),
            })
        );
    }

    #[test]
    fn policy_feedback_rejects_unbounded_duplicate_and_future_evidence() {
        let subject = RuntimePolicySubject::WorkItem {
            attempt_id: "attempt-1".into(),
            item_id: "item-1".into(),
            item_revision: 1,
            objective: "Inspect the canonical runtime feedback path".into(),
            expected_result: "The typed frame is projected unchanged".into(),
        };
        let entry = RuntimePolicyFeedbackEntry {
            signal: RuntimePolicySignal::RedundantReads,
            stage: RuntimePolicyStage::Observe,
            observed_at_round: 3,
            evidence_count: 8,
            recommendation: RuntimePolicyRecommendation::ReuseKnownContent,
        };
        let evaluated = |subject, entries| RuntimePolicyFeedbackSet::Evaluated {
            schema_version: RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            revision: 1,
            evaluated_at_round: 3,
            subject,
            entries,
        };

        assert!(evaluated(subject.clone(), vec![entry.clone()]).is_valid(3));
        assert!(!evaluated(subject.clone(), vec![entry.clone(), entry.clone()]).is_valid(3));

        let mut future = entry.clone();
        future.observed_at_round = 4;
        assert!(!evaluated(subject.clone(), vec![future]).is_valid(3));

        let mut unbounded = subject;
        let RuntimePolicySubject::WorkItem { objective, .. } = &mut unbounded else {
            unreachable!();
        };
        *objective = "x".repeat(2_001);
        assert!(!evaluated(unbounded, vec![entry]).is_valid(3));
    }

    #[test]
    fn policy_feedback_preserves_unknown_vs_evaluated_without_advisory() {
        let evaluated = RuntimePolicyFeedbackSet::Evaluated {
            schema_version: RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            revision: 1,
            evaluated_at_round: 0,
            subject: RuntimePolicySubject::Run,
            entries: Vec::new(),
        };

        assert_ne!(RuntimePolicyFeedbackSet::NotEvaluated, evaluated);
        assert_eq!(
            RuntimePolicyFeedbackSet::NotEvaluated.inline_summary(),
            "not_evaluated"
        );
        assert_eq!(evaluated.inline_summary(), "run:none");
    }

    #[test]
    fn runtime_feedback_requires_typed_topology_and_latest_schema() {
        let mut frame = crate::introspect::test_runtime_feedback(2, 3, 7);
        frame.identity.topology = astra_services::ModelRequestTopology::CliServer;
        let mut wire = serde_json::to_value(&frame).expect("runtime frame serializes");
        assert_eq!(wire["identity"]["topology"], "cli_server");

        wire["schema_version"] = serde_json::json!(3);
        let stale: RuntimeFeedbackFrame =
            serde_json::from_value(wire.clone()).expect("typed stale frame decodes");
        assert!(
            !stale.is_valid(),
            "stale schema must never become authority"
        );

        wire["schema_version"] = serde_json::json!(RuntimeFeedbackFrame::SCHEMA_VERSION);
        wire["identity"]
            .as_object_mut()
            .expect("identity object")
            .remove("topology");
        assert!(serde_json::from_value::<RuntimeFeedbackFrame>(wire).is_err());
    }

    #[test]
    fn runtime_feedback_marks_active_execution_signals_without_making_them_hard() {
        let mut frame = crate::introspect::test_runtime_feedback(2, 3, 7);
        frame.policy_feedback = RuntimePolicyFeedbackSet::Evaluated {
            schema_version: RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            revision: 2,
            evaluated_at_round: 3,
            subject: RuntimePolicySubject::Run,
            entries: vec![RuntimePolicyFeedbackEntry {
                signal: RuntimePolicySignal::UnresolvedToolOutcomes,
                stage: RuntimePolicyStage::Observe,
                observed_at_round: 3,
                evidence_count: 1,
                recommendation: RuntimePolicyRecommendation::DiagnoseToolOutcomes,
            }],
        };
        assert!(!frame.has_unresolved_tool_outcomes());

        if let RuntimePolicyFeedbackSet::Evaluated { entries, .. } = &mut frame.policy_feedback {
            entries[0].stage = RuntimePolicyStage::Converge;
        }
        assert!(frame.has_unresolved_tool_outcomes());

        frame.policy_feedback = RuntimePolicyFeedbackSet::Evaluated {
            schema_version: RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            revision: 3,
            evaluated_at_round: 3,
            subject: RuntimePolicySubject::Run,
            entries: Vec::new(),
        };
        assert!(!frame.has_unresolved_tool_outcomes());
    }
}
