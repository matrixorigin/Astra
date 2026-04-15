use astra_core::confidence::ConfidenceInterval;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::evaluation::types::ValueInterval;
use crate::{SubtaskVerificationReport, VerificationResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationActionCategory {
    Read,
    Write,
    Execute,
    Destructive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationCompensationPolicy {
    pub bounded: bool,
    pub reversible: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_pre_state: bool,
    pub action_category: MutationActionCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation_summary: Option<String>,
}

impl MutationCompensationPolicy {
    pub fn read() -> Self {
        Self {
            bounded: true,
            reversible: true,
            requires_pre_state: false,
            action_category: MutationActionCategory::Read,
            compensation_kind: None,
            compensation_summary: None,
        }
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn format_value_interval_evidence(
    label: &str,
    value: f64,
    interval: Option<&ValueInterval>,
) -> String {
    if let Some(interval) = interval {
        format!(
            "{label}={value:.2}[{:.2},{:.2}]",
            interval.lower, interval.upper
        )
    } else {
        format!("{label}={value:.2}")
    }
}

fn format_confidence_interval_evidence(label: &str, interval: ConfidenceInterval) -> String {
    format!(
        "{label}={:.2}[{:.2},{:.2}]",
        interval.point, interval.lower, interval.upper
    )
}

fn interval_floor(value: f64, interval: Option<&ValueInterval>) -> f64 {
    interval.map_or(value, |interval| interval.lower)
}

fn interval_ceiling(value: f64, interval: Option<&ValueInterval>) -> f64 {
    interval.map_or(value, |interval| interval.upper)
}

fn complement_confidence_interval(interval: ConfidenceInterval) -> ConfidenceInterval {
    ConfidenceInterval::new(
        1.0 - interval.point,
        1.0 - interval.upper,
        1.0 - interval.lower,
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationObjectiveScore {
    pub quality: ConfidenceInterval,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_feedback: Option<ConfidenceInterval>,
    pub reward_hacking_risk: ConfidenceInterval,
    pub causal_support: ConfidenceInterval,
    #[serde(default)]
    pub was_corrected: bool,
}

impl MutationObjectiveScore {
    pub fn new(
        quality: ConfidenceInterval,
        user_feedback: Option<ConfidenceInterval>,
        reward_hacking_risk: ConfidenceInterval,
        causal_support: ConfidenceInterval,
        was_corrected: bool,
    ) -> Self {
        Self {
            quality,
            user_feedback,
            reward_hacking_risk,
            causal_support,
            was_corrected,
        }
    }

    pub fn from_learning_signal(
        quality: f64,
        user_feedback_score: Option<i64>,
        reward_hacking_risk: f64,
        causal_support: f64,
        was_corrected: bool,
    ) -> Self {
        Self::new(
            ConfidenceInterval::exact(quality),
            user_feedback_score.map(normalized_feedback_interval),
            ConfidenceInterval::exact(reward_hacking_risk),
            ConfidenceInterval::exact(causal_support),
            was_corrected,
        )
    }

    pub fn retention_score(&self) -> ConfidenceInterval {
        let mut score = self
            .quality
            .min(complement_confidence_interval(self.reward_hacking_risk))
            .min(self.causal_support);
        if let Some(feedback) = self.user_feedback {
            score = score.min(feedback);
        }
        if self.was_corrected {
            score = score.min(ConfidenceInterval::exact(0.25));
        }
        score
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationVerifierSummary {
    pub all_required_passed: bool,
    pub criteria_total: u32,
    pub criteria_passed: u32,
    pub pass_rate: ConfidenceInterval,
    pub failing_criteria: Vec<String>,
}

impl MutationVerifierSummary {
    pub fn from_report(report: &SubtaskVerificationReport) -> Self {
        Self::from_results(report.all_required_passed, &report.results)
    }

    pub fn from_results(all_required_passed: bool, results: &[VerificationResult]) -> Self {
        let criteria_total = results.len() as u32;
        let criteria_passed = results.iter().filter(|result| result.passed).count() as u32;
        let pass_rate = if criteria_total == 0 {
            ConfidenceInterval::FULL
        } else {
            ConfidenceInterval::exact(criteria_passed as f64 / criteria_total as f64)
        };
        let failing_criteria = results
            .iter()
            .filter(|result| !result.passed)
            .map(|result| result.criterion_id.clone())
            .collect();
        Self {
            all_required_passed,
            criteria_total,
            criteria_passed,
            pass_rate,
            failing_criteria,
        }
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationSafetyVerdict {
    Safe,
    RequiresApproval,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationRetentionVerdict {
    Retain,
    Review,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MutationPromotionRecommendation {
    Promote,
    Canary,
    #[default]
    Hold,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MutationPromotionVerdict {
    pub recommendation: MutationPromotionRecommendation,
    pub confidence_score: f64,
    pub support_score: f64,
    pub safety_score: f64,
    pub overall_score: f64,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MutationPromotionEvaluationContext {
    pub noise_filtered_quality: Option<ConfidenceInterval>,
    pub latest_gate_passed: Option<bool>,
    pub latest_gate_score_delta: Option<f64>,
    pub latest_gate_score_delta_interval: Option<ValueInterval>,
    pub calibration_error: Option<f64>,
    pub calibration_error_interval: Option<ValueInterval>,
    pub missing_verifier_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationJudgment {
    pub retention_score: ConfidenceInterval,
    pub verifier_pass_rate: ConfidenceInterval,
    pub safety_verdict: MutationSafetyVerdict,
    pub retention_verdict: MutationRetentionVerdict,
    #[serde(default)]
    pub promotion_verdict: MutationPromotionVerdict,
    pub rationale: Vec<String>,
}

impl MutationJudgment {
    pub fn evaluate(
        objective: &MutationObjectiveScore,
        verifier: Option<&MutationVerifierSummary>,
        compensation: &MutationCompensationPolicy,
        pre_state_snapshot_id: Option<&str>,
        pre_state_snapshot_database: Option<&str>,
    ) -> Self {
        let has_pre_state_snapshot = pre_state_snapshot_id.is_some();
        let retention_score = objective.retention_score();
        let verifier_pass_rate = verifier
            .map(|summary| summary.pass_rate)
            .unwrap_or(ConfidenceInterval::FULL);
        let all_required_passed = verifier
            .map(|summary| summary.all_required_passed)
            .unwrap_or(true);
        let mut rationale = Vec::new();

        let safety_verdict = if !all_required_passed {
            rationale.push("required_verifiers_failed".to_string());
            MutationSafetyVerdict::Blocked
        } else if compensation.requires_pre_state && !has_pre_state_snapshot {
            rationale.push("missing_pre_state_snapshot".to_string());
            MutationSafetyVerdict::RequiresApproval
        } else if !compensation.reversible || !compensation.bounded {
            rationale.push("requires_staged_approval".to_string());
            MutationSafetyVerdict::RequiresApproval
        } else {
            rationale.push("staged_apply_ready".to_string());
            MutationSafetyVerdict::Safe
        };

        let retention_verdict = if !all_required_passed || retention_score.upper < 0.4 {
            rationale.push("retain_score_below_threshold".to_string());
            MutationRetentionVerdict::Reject
        } else if retention_score.lower >= 0.6 && verifier_pass_rate.lower >= 0.7 {
            MutationRetentionVerdict::Retain
        } else {
            rationale.push("retain_score_requires_review".to_string());
            MutationRetentionVerdict::Review
        };

        rationale.sort();
        rationale.dedup();
        let promotion_verdict = build_mutation_promotion_verdict(
            retention_score,
            verifier,
            safety_verdict,
            retention_verdict,
            compensation,
            has_pre_state_snapshot,
            pre_state_snapshot_id,
            pre_state_snapshot_database,
            &rationale,
        );
        Self {
            retention_score,
            verifier_pass_rate,
            safety_verdict,
            retention_verdict,
            promotion_verdict,
            rationale,
        }
    }

    pub fn staged_state(&self) -> StagedMutationState {
        match self.promotion_verdict.recommendation {
            MutationPromotionRecommendation::Promote => StagedMutationState::Ready,
            MutationPromotionRecommendation::Canary => StagedMutationState::Pending,
            MutationPromotionRecommendation::Hold => {
                if matches!(self.retention_verdict, MutationRetentionVerdict::Reject)
                    || matches!(self.safety_verdict, MutationSafetyVerdict::Blocked)
                {
                    StagedMutationState::Blocked
                } else {
                    StagedMutationState::Pending
                }
            }
        }
    }

    pub fn apply_promotion_context(
        &mut self,
        context: &MutationPromotionEvaluationContext,
        verifier_present: bool,
    ) {
        if let Some(quality) = context.noise_filtered_quality {
            self.promotion_verdict.evidence.push(format!(
                "noise_filtered_quality={:.2}[{:.2},{:.2}]",
                quality.point, quality.lower, quality.upper
            ));
            self.promotion_verdict.confidence_score =
                self.promotion_verdict.confidence_score.min(quality.lower);
            if quality.lower < 0.55 {
                self.promotion_verdict
                    .blockers
                    .push("noise_filtered_quality_low".into());
            }
        }
        if let Some(passed) = context.latest_gate_passed {
            self.promotion_verdict
                .evidence
                .push(format!("latest_gate_passed={passed}"));
            if !passed {
                self.promotion_verdict.support_score =
                    self.promotion_verdict.support_score.min(0.60);
                self.promotion_verdict.safety_score = self.promotion_verdict.safety_score.min(0.60);
            }
        }
        if let Some(score_delta) = context.latest_gate_score_delta {
            let delta_interval = context.latest_gate_score_delta_interval.as_ref();
            let delta_floor = interval_floor(score_delta, delta_interval);
            self.promotion_verdict
                .evidence
                .push(format_value_interval_evidence(
                    "latest_gate_score_delta",
                    score_delta,
                    delta_interval,
                ));
            if delta_floor < -0.15 {
                self.promotion_verdict
                    .blockers
                    .push("latest_gate_regression".into());
            } else if delta_floor < -0.05 {
                self.promotion_verdict.support_score =
                    self.promotion_verdict.support_score.min(0.65);
            }
        }
        if let Some(calibration_error) = context.calibration_error {
            let calibration_interval = context.calibration_error_interval.as_ref();
            let calibration_ceiling = interval_ceiling(calibration_error, calibration_interval);
            self.promotion_verdict
                .evidence
                .push(format_value_interval_evidence(
                    "calibration_error",
                    calibration_error,
                    calibration_interval,
                ));
            if calibration_ceiling > 0.35 {
                self.promotion_verdict
                    .blockers
                    .push("calibration_error_high".into());
            } else if calibration_ceiling > 0.20 {
                self.promotion_verdict.support_score =
                    self.promotion_verdict.support_score.min(0.60);
            }
        }
        if let Some(missing_verifier_rate) = context.missing_verifier_rate {
            self.promotion_verdict
                .evidence
                .push(format!("missing_verifier_rate={missing_verifier_rate:.2}"));
            if !verifier_present {
                self.promotion_verdict.support_score = self
                    .promotion_verdict
                    .support_score
                    .min((0.75 - missing_verifier_rate).clamp(0.35, 0.55));
            }
        }

        self.promotion_verdict.evidence.sort();
        self.promotion_verdict.evidence.dedup();
        self.promotion_verdict.blockers.sort();
        self.promotion_verdict.blockers.dedup();
        self.promotion_verdict.overall_score = (self.promotion_verdict.confidence_score * 0.45
            + self.promotion_verdict.support_score * 0.30
            + self.promotion_verdict.safety_score * 0.25)
            .clamp(0.0, 1.0);
        self.promotion_verdict.recommendation = compute_mutation_promotion_recommendation(
            &self.promotion_verdict.blockers,
            verifier_present,
            self.retention_verdict,
            self.safety_verdict,
            self.promotion_verdict.support_score,
            self.promotion_verdict.overall_score,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagedMutationState {
    Pending,
    Ready,
    Applied,
    Reverted,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StagedMutation {
    pub mutation_id: String,
    pub session_id: String,
    pub turn_index: u32,
    pub tool_name: String,
    pub tool_args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_state_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_state_snapshot_database: Option<String>,
    pub state: StagedMutationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updated_at: Option<String>,
    pub objective: MutationObjectiveScore,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<MutationVerifierSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_gap: Option<String>,
    pub compensation: MutationCompensationPolicy,
    pub judgment: MutationJudgment,
}

impl StagedMutation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mutation_id: impl Into<String>,
        session_id: impl Into<String>,
        turn_index: u32,
        tool_name: impl Into<String>,
        tool_args: Value,
        pre_state_snapshot_id: Option<String>,
        objective: MutationObjectiveScore,
        verifier: Option<MutationVerifierSummary>,
        compensation: MutationCompensationPolicy,
    ) -> Self {
        Self::new_with_pre_state_snapshot_database(
            mutation_id,
            session_id,
            turn_index,
            tool_name,
            tool_args,
            pre_state_snapshot_id,
            None,
            objective,
            verifier,
            compensation,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_pre_state_snapshot_database(
        mutation_id: impl Into<String>,
        session_id: impl Into<String>,
        turn_index: u32,
        tool_name: impl Into<String>,
        tool_args: Value,
        pre_state_snapshot_id: Option<String>,
        pre_state_snapshot_database: Option<String>,
        objective: MutationObjectiveScore,
        verifier: Option<MutationVerifierSummary>,
        compensation: MutationCompensationPolicy,
    ) -> Self {
        let pre_state_snapshot_database = pre_state_snapshot_database.or_else(|| {
            tool_args
                .get("database")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|database| !database.is_empty())
                .map(ToString::to_string)
        });
        let judgment = MutationJudgment::evaluate(
            &objective,
            verifier.as_ref(),
            &compensation,
            pre_state_snapshot_id.as_deref(),
            pre_state_snapshot_database.as_deref(),
        );
        let state = judgment.staged_state();
        Self {
            mutation_id: mutation_id.into(),
            session_id: session_id.into(),
            turn_index,
            tool_name: tool_name.into(),
            tool_args,
            pre_state_snapshot_id,
            pre_state_snapshot_database,
            state,
            state_note: None,
            state_updated_at: None,
            objective,
            verifier,
            verifier_source: None,
            verifier_gap: None,
            compensation,
            judgment,
        }
    }

    pub fn apply_promotion_context(&mut self, context: &MutationPromotionEvaluationContext) {
        self.judgment
            .apply_promotion_context(context, self.verifier.is_some());
        if self.state_updated_at.is_none() {
            self.state = self.judgment.staged_state();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_mutation_promotion_verdict(
    retention_score: ConfidenceInterval,
    verifier: Option<&MutationVerifierSummary>,
    safety_verdict: MutationSafetyVerdict,
    retention_verdict: MutationRetentionVerdict,
    compensation: &MutationCompensationPolicy,
    has_pre_state_snapshot: bool,
    pre_state_snapshot_id: Option<&str>,
    pre_state_snapshot_database: Option<&str>,
    rationale: &[String],
) -> MutationPromotionVerdict {
    let mut evidence = vec![format_confidence_interval_evidence(
        "retention_score",
        retention_score,
    )];
    let mut blockers = Vec::new();

    let support_score = if let Some(summary) = verifier {
        evidence.push(format!(
            "{} ({}/{})",
            format_confidence_interval_evidence("verifier_pass_rate", summary.pass_rate),
            summary.criteria_passed,
            summary.criteria_total
        ));
        if !summary.all_required_passed {
            blockers.push(format!(
                "required_verifiers_failed:{}",
                summary.failing_criteria.join(",")
            ));
        }
        summary.pass_rate.lower
    } else {
        evidence.push("no_structured_verifier_signal".into());
        0.55
    };

    let mut safety_score: f64 = match safety_verdict {
        MutationSafetyVerdict::Safe => 0.90,
        MutationSafetyVerdict::RequiresApproval => 0.65,
        MutationSafetyVerdict::Blocked => 0.25,
    };
    if compensation.bounded && compensation.reversible {
        evidence.push("bounded_reversible_compensation".into());
    } else {
        evidence.push("manual_or_unbounded_rollback".into());
    }
    if compensation.requires_pre_state && !has_pre_state_snapshot {
        evidence.push("missing_pre_state_snapshot".into());
        safety_score = safety_score.min(0.60);
    }

    if matches!(retention_verdict, MutationRetentionVerdict::Reject) {
        blockers.push("retention_score_below_threshold".into());
    }
    if matches!(safety_verdict, MutationSafetyVerdict::Blocked) {
        blockers.push("mutation_safety_blocked".into());
    }

    let rollback_hint = if compensation.compensation_kind.as_deref() == Some("restore_database_snapshot")
    {
        pre_state_snapshot_id.map(|snapshot_id| match pre_state_snapshot_database {
            Some(database) if !database.is_empty() => format!(
                "call `rollback_database_snapshots` with scope=`snapshot`, snapshot_id=`{snapshot_id}`, and database=`{database}` to restore the captured pre-state snapshot"
            ),
            _ => format!(
                "call `rollback_database_snapshots` with scope=`snapshot` and snapshot_id=`{snapshot_id}` to restore the captured pre-state snapshot"
            ),
        })
    } else {
        None
    }
    .or_else(|| compensation.compensation_summary.clone())
    .or_else(|| pre_state_snapshot_id.map(|snapshot_id| format!("restore snapshot {snapshot_id}")));
    let confidence_score = retention_score.lower;
    let overall_score =
        (confidence_score * 0.45 + support_score * 0.30 + safety_score * 0.25).clamp(0.0, 1.0);

    let recommendation = compute_mutation_promotion_recommendation(
        &blockers,
        verifier.is_some(),
        retention_verdict,
        safety_verdict,
        support_score,
        overall_score,
    );

    let mut all_evidence = rationale.to_vec();
    all_evidence.extend(evidence);
    all_evidence.sort();
    all_evidence.dedup();

    MutationPromotionVerdict {
        recommendation,
        confidence_score,
        support_score,
        safety_score,
        overall_score,
        evidence: all_evidence,
        blockers,
        rollback_hint,
    }
}

fn compute_mutation_promotion_recommendation(
    blockers: &[String],
    verifier_present: bool,
    retention_verdict: MutationRetentionVerdict,
    safety_verdict: MutationSafetyVerdict,
    support_score: f64,
    overall_score: f64,
) -> MutationPromotionRecommendation {
    if blockers.is_empty()
        && verifier_present
        && matches!(retention_verdict, MutationRetentionVerdict::Retain)
        && matches!(safety_verdict, MutationSafetyVerdict::Safe)
        && support_score >= 0.70
        && overall_score >= 0.78
    {
        MutationPromotionRecommendation::Promote
    } else if !matches!(retention_verdict, MutationRetentionVerdict::Reject)
        && !matches!(safety_verdict, MutationSafetyVerdict::Blocked)
        && overall_score >= 0.55
    {
        MutationPromotionRecommendation::Canary
    } else {
        MutationPromotionRecommendation::Hold
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PersistedMutationDecision {
    pub decision_id: String,
    pub session_id: String,
    pub decision_output: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationScoreboard {
    pub scoreboard_id: String,
    pub session_id: String,
    pub total_mutations: u32,
    pub ready_mutations: u32,
    pub approval_required_mutations: u32,
    pub applied_mutations: u32,
    pub reverted_mutations: u32,
    pub blocked_mutations: u32,
    pub avg_objective_quality: ConfidenceInterval,
    pub avg_retention_score: ConfidenceInterval,
    pub avg_verifier_pass_rate: ConfidenceInterval,
    pub avg_reward_hacking_risk: ConfidenceInterval,
    pub avg_causal_support: ConfidenceInterval,
    pub mutations: Vec<StagedMutation>,
}

impl MutationScoreboard {
    pub fn new(
        scoreboard_id: impl Into<String>,
        session_id: impl Into<String>,
        mutations: Vec<StagedMutation>,
    ) -> Self {
        let ready_mutations = mutations
            .iter()
            .filter(|mutation| mutation.state == StagedMutationState::Ready)
            .count() as u32;
        let approval_required_mutations = mutations
            .iter()
            .filter(|mutation| {
                mutation.state == StagedMutationState::Pending
                    && mutation.judgment.safety_verdict == MutationSafetyVerdict::RequiresApproval
            })
            .count() as u32;
        let applied_mutations = mutations
            .iter()
            .filter(|mutation| mutation.state == StagedMutationState::Applied)
            .count() as u32;
        let reverted_mutations = mutations
            .iter()
            .filter(|mutation| mutation.state == StagedMutationState::Reverted)
            .count() as u32;
        let blocked_mutations = mutations
            .iter()
            .filter(|mutation| mutation.state == StagedMutationState::Blocked)
            .count() as u32;

        Self {
            scoreboard_id: scoreboard_id.into(),
            session_id: session_id.into(),
            total_mutations: mutations.len() as u32,
            ready_mutations,
            approval_required_mutations,
            applied_mutations,
            reverted_mutations,
            blocked_mutations,
            avg_objective_quality: average_confidence(
                mutations.iter().map(|mutation| mutation.objective.quality),
            ),
            avg_retention_score: average_confidence(
                mutations
                    .iter()
                    .map(|mutation| mutation.judgment.retention_score),
            ),
            avg_verifier_pass_rate: average_confidence(
                mutations
                    .iter()
                    .map(|mutation| mutation.judgment.verifier_pass_rate),
            ),
            avg_reward_hacking_risk: average_confidence(
                mutations
                    .iter()
                    .map(|mutation| mutation.objective.reward_hacking_risk),
            ),
            avg_causal_support: average_confidence(
                mutations
                    .iter()
                    .map(|mutation| mutation.objective.causal_support),
            ),
            mutations,
        }
    }

    pub fn from_persisted_decisions(
        scoreboard_id: impl Into<String>,
        session_id: impl Into<String>,
        decisions: impl IntoIterator<Item = PersistedMutationDecision>,
    ) -> Self {
        let session_id = session_id.into();
        let mutations = decisions
            .into_iter()
            .flat_map(staged_mutations_from_persisted_decision)
            .collect::<Vec<_>>();
        Self::new(scoreboard_id, session_id, mutations)
    }

    pub fn with_promotion_context(mut self, context: &MutationPromotionEvaluationContext) -> Self {
        for mutation in &mut self.mutations {
            mutation.apply_promotion_context(context);
        }
        Self::new(self.scoreboard_id, self.session_id, self.mutations)
    }
}

fn normalized_feedback_interval(score: i64) -> ConfidenceInterval {
    ConfidenceInterval::exact((score as f64 / 100.0).clamp(0.0, 1.0))
}

fn staged_mutations_from_persisted_decision(
    decision: PersistedMutationDecision,
) -> Vec<StagedMutation> {
    let Some(objective) = decision
        .decision_output
        .get("mutation_objective_score")
        .and_then(MutationObjectiveScore::from_value)
    else {
        return Vec::new();
    };
    let turn_index = decision
        .decision_output
        .get("turn")
        .and_then(Value::as_u64)
        .unwrap_or_default() as u32;
    let Some(action_profiles) = decision
        .decision_output
        .get("action_profiles")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    action_profiles
        .iter()
        .enumerate()
        .filter_map(|(index, action_profile)| {
            let tool_name = action_profile.get("tool_name").and_then(Value::as_str)?;
            let compensation = action_profile
                .get("profile")
                .and_then(MutationCompensationPolicy::from_value)?;
            let tool_call_id = action_profile
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mutation_id = if tool_call_id.is_empty() {
                format!("{}:{index}", decision.decision_id)
            } else {
                format!("{}:{tool_call_id}", decision.decision_id)
            };
            let mut mutation = StagedMutation::new_with_pre_state_snapshot_database(
                mutation_id,
                decision.session_id.clone(),
                turn_index,
                tool_name,
                action_profile
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default())),
                action_profile
                    .get("pre_state_snapshot_id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                action_profile
                    .get("pre_state_snapshot_database")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                objective.clone(),
                action_profile
                    .get("verifier")
                    .and_then(MutationVerifierSummary::from_value),
                compensation,
            );
            mutation.verifier_source = action_profile
                .get("verifier_source")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            mutation.verifier_gap = action_profile
                .get("verifier_gap")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            Some(mutation)
        })
        .collect()
}

fn average_confidence(values: impl Iterator<Item = ConfidenceInterval>) -> ConfidenceInterval {
    let values: Vec<_> = values.collect();
    if values.is_empty() {
        return ConfidenceInterval::ZERO;
    }
    let count = values.len() as f64;
    ConfidenceInterval::new(
        values.iter().map(|value| value.point).sum::<f64>() / count,
        values.iter().map(|value| value.lower).sum::<f64>() / count,
        values.iter().map(|value| value.upper).sum::<f64>() / count,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VerificationResult;

    fn verification_report(all_required_passed: bool) -> SubtaskVerificationReport {
        SubtaskVerificationReport {
            subtask_id: "subtask-1".into(),
            all_required_passed,
            results: vec![
                VerificationResult {
                    criterion_id: "build".into(),
                    passed: true,
                    evidence: "cargo check ok".into(),
                    expected: "exit 0".into(),
                    duration_ms: 1200,
                    error: None,
                },
                VerificationResult {
                    criterion_id: "tests".into(),
                    passed: all_required_passed,
                    evidence: if all_required_passed {
                        "all tests passed".into()
                    } else {
                        "2 tests failed".into()
                    },
                    expected: "all required tests pass".into(),
                    duration_ms: 2400,
                    error: None,
                },
            ],
            timestamp: "2026-04-12T00:00:00Z".into(),
        }
    }

    fn automated_policy(requires_pre_state: bool) -> MutationCompensationPolicy {
        MutationCompensationPolicy {
            bounded: true,
            reversible: true,
            requires_pre_state,
            action_category: MutationActionCategory::Write,
            compensation_kind: Some("restore_or_delete_file".into()),
            compensation_summary: Some("restore prior contents".into()),
        }
    }

    fn database_snapshot_policy() -> MutationCompensationPolicy {
        MutationCompensationPolicy {
            bounded: true,
            reversible: true,
            requires_pre_state: true,
            action_category: MutationActionCategory::Write,
            compensation_kind: Some("restore_database_snapshot".into()),
            compensation_summary: Some(
                "call `mo_snapshot` with action=`restore` and the captured snapshot name".into(),
            ),
        }
    }

    #[test]
    fn retention_score_uses_hardest_signal() {
        let objective =
            MutationObjectiveScore::from_learning_signal(0.92, Some(81), 0.15, 0.74, false);
        assert_eq!(objective.retention_score(), ConfidenceInterval::exact(0.74));
    }

    #[test]
    fn retention_score_penalizes_correction() {
        let objective =
            MutationObjectiveScore::from_learning_signal(0.92, Some(81), 0.15, 0.74, true);
        assert_eq!(objective.retention_score(), ConfidenceInterval::exact(0.25));
    }

    #[test]
    fn retention_score_preserves_reward_hacking_interval_bounds() {
        let objective = MutationObjectiveScore::new(
            ConfidenceInterval::exact(0.95),
            None,
            ConfidenceInterval::new(0.20, 0.10, 0.40),
            ConfidenceInterval::exact(0.90),
            false,
        );

        assert_eq!(
            objective.retention_score(),
            ConfidenceInterval::new(0.80, 0.60, 0.90)
        );
    }

    #[test]
    fn verifier_summary_tracks_failures() {
        let summary = MutationVerifierSummary::from_report(&verification_report(false));
        assert!(!summary.all_required_passed);
        assert_eq!(summary.criteria_total, 2);
        assert_eq!(summary.criteria_passed, 1);
        assert_eq!(summary.pass_rate, ConfidenceInterval::exact(0.5));
        assert_eq!(summary.failing_criteria, vec!["tests".to_string()]);
    }

    #[test]
    fn staged_mutation_is_ready_when_signals_are_strong() {
        let mutation = StagedMutation::new(
            "mut-1",
            "session-1",
            3,
            "write_file",
            serde_json::json!({"path": "src/lib.rs"}),
            Some("snap-1".into()),
            MutationObjectiveScore::from_learning_signal(0.93, Some(88), 0.1, 0.82, false),
            Some(MutationVerifierSummary::from_report(&verification_report(
                true,
            ))),
            automated_policy(true),
        );
        assert_eq!(mutation.state, StagedMutationState::Ready);
        assert_eq!(
            mutation.judgment.safety_verdict,
            MutationSafetyVerdict::Safe
        );
        assert_eq!(
            mutation.judgment.retention_verdict,
            MutationRetentionVerdict::Retain
        );
        assert_eq!(
            mutation.judgment.promotion_verdict.recommendation,
            MutationPromotionRecommendation::Promote
        );
    }

    #[test]
    fn staged_mutation_requires_approval_without_pre_state_snapshot() {
        let mutation = StagedMutation::new(
            "mut-2",
            "session-1",
            4,
            "write_file",
            serde_json::json!({"path": "src/lib.rs"}),
            None,
            MutationObjectiveScore::from_learning_signal(0.9, Some(85), 0.1, 0.8, false),
            Some(MutationVerifierSummary::from_report(&verification_report(
                true,
            ))),
            automated_policy(true),
        );
        assert_eq!(mutation.state, StagedMutationState::Pending);
        assert_eq!(
            mutation.judgment.safety_verdict,
            MutationSafetyVerdict::RequiresApproval
        );
        assert!(
            mutation
                .judgment
                .rationale
                .iter()
                .any(|reason| reason == "missing_pre_state_snapshot")
        );
        assert_eq!(
            mutation.judgment.promotion_verdict.recommendation,
            MutationPromotionRecommendation::Canary
        );
    }

    #[test]
    fn staged_mutation_without_verifier_stays_canary_pending() {
        let mutation = StagedMutation::new(
            "mut-3",
            "session-1",
            5,
            "write_file",
            serde_json::json!({"path": "src/lib.rs"}),
            Some("snap-3".into()),
            MutationObjectiveScore::from_learning_signal(0.94, Some(90), 0.05, 0.86, false),
            None,
            automated_policy(true),
        );

        assert_eq!(mutation.state, StagedMutationState::Pending);
        assert_eq!(
            mutation.judgment.promotion_verdict.recommendation,
            MutationPromotionRecommendation::Canary
        );
        assert!(
            mutation
                .judgment
                .promotion_verdict
                .evidence
                .iter()
                .any(|evidence| evidence == "no_structured_verifier_signal")
        );
    }

    #[test]
    fn database_snapshot_rollback_hint_prefers_concrete_snapshot_restore() {
        let mutation = StagedMutation::new_with_pre_state_snapshot_database(
            "mut-db",
            "session-1",
            6,
            "mo_query",
            serde_json::json!({"sql": "UPDATE metrics SET value = 1"}),
            Some("moq_snap_9".into()),
            Some("analytics".into()),
            MutationObjectiveScore::from_learning_signal(0.93, Some(88), 0.08, 0.82, false),
            Some(MutationVerifierSummary::from_report(&verification_report(
                true,
            ))),
            database_snapshot_policy(),
        );

        assert_eq!(
            mutation.judgment.promotion_verdict.rollback_hint.as_deref(),
            Some(
                "call `rollback_database_snapshots` with scope=`snapshot`, snapshot_id=`moq_snap_9`, and database=`analytics` to restore the captured pre-state snapshot"
            )
        );
        assert_eq!(
            mutation.judgment.safety_verdict,
            MutationSafetyVerdict::Safe
        );
    }

    #[test]
    fn session_state_compensation_stays_staged_apply_ready_without_snapshot() {
        let mutation = StagedMutation::new(
            "mut-session",
            "session-1",
            7,
            "adjust_config",
            serde_json::json!({"path": "memory.retrieval_top_k", "value": 6}),
            None,
            MutationObjectiveScore::from_learning_signal(0.91, Some(86), 0.04, 0.83, false),
            Some(MutationVerifierSummary::from_report(&verification_report(
                true,
            ))),
            MutationCompensationPolicy {
                bounded: true,
                reversible: true,
                requires_pre_state: false,
                action_category: MutationActionCategory::Write,
                compensation_kind: Some("restore_session_state".into()),
                compensation_summary: Some(
                    "call `rollback_session_state` with scope=`current_turn` to restore the previous session state"
                        .into(),
                ),
            },
        );

        assert_eq!(
            mutation.judgment.safety_verdict,
            MutationSafetyVerdict::Safe
        );
        assert!(
            mutation
                .judgment
                .promotion_verdict
                .evidence
                .iter()
                .any(|evidence| evidence == "bounded_reversible_compensation")
        );
        assert!(
            mutation
                .judgment
                .promotion_verdict
                .evidence
                .iter()
                .all(|evidence| evidence != "missing_pre_state_snapshot")
        );
        assert_eq!(
            mutation.judgment.promotion_verdict.rollback_hint.as_deref(),
            Some(
                "call `rollback_session_state` with scope=`current_turn` to restore the previous session state"
            )
        );
    }

    #[test]
    fn uncertain_retention_score_stays_review_instead_of_reject() {
        let mutation = StagedMutation::new(
            "mut-uncertain",
            "session-1",
            6,
            "write_file",
            serde_json::json!({"path": "src/lib.rs"}),
            Some("snap-6".into()),
            MutationObjectiveScore::new(
                ConfidenceInterval::new(0.38, 0.28, 0.48),
                None,
                ConfidenceInterval::exact(0.05),
                ConfidenceInterval::exact(0.90),
                false,
            ),
            Some(MutationVerifierSummary::from_report(&verification_report(
                true,
            ))),
            automated_policy(true),
        );

        assert_eq!(
            mutation.judgment.retention_verdict,
            MutationRetentionVerdict::Review
        );
        assert!(
            mutation
                .judgment
                .rationale
                .iter()
                .any(|reason| reason == "retain_score_requires_review")
        );
    }

    #[test]
    fn promotion_verdict_uses_lower_bounds_for_scalar_scores() {
        let mutation = StagedMutation::new(
            "mut-lower-bounds",
            "session-1",
            7,
            "write_file",
            serde_json::json!({"path": "src/lib.rs"}),
            Some("snap-7".into()),
            MutationObjectiveScore::new(
                ConfidenceInterval::new(0.78, 0.64, 0.88),
                None,
                ConfidenceInterval::exact(0.05),
                ConfidenceInterval::exact(0.90),
                false,
            ),
            Some(MutationVerifierSummary {
                all_required_passed: true,
                criteria_total: 3,
                criteria_passed: 3,
                pass_rate: ConfidenceInterval::new(0.90, 0.72, 0.98),
                failing_criteria: Vec::new(),
            }),
            automated_policy(true),
        );

        assert!((mutation.judgment.promotion_verdict.confidence_score - 0.64).abs() < 0.0001);
        assert!((mutation.judgment.promotion_verdict.support_score - 0.72).abs() < 0.0001);
        assert!(
            mutation
                .judgment
                .promotion_verdict
                .evidence
                .iter()
                .any(|evidence| evidence.contains("retention_score=0.78[0.64,0.88]"))
        );
    }

    #[test]
    fn scoreboard_aggregates_mutation_counts_and_scores() {
        let ready = StagedMutation::new(
            "mut-1",
            "session-1",
            1,
            "write_file",
            serde_json::json!({"path": "src/lib.rs"}),
            Some("snap-1".into()),
            MutationObjectiveScore::from_learning_signal(0.9, Some(90), 0.1, 0.8, false),
            Some(MutationVerifierSummary::from_report(&verification_report(
                true,
            ))),
            automated_policy(true),
        );
        let mut reverted = StagedMutation::new(
            "mut-2",
            "session-1",
            2,
            "bash",
            serde_json::json!({"command": "git commit -m 'x'"}),
            Some("snap-2".into()),
            MutationObjectiveScore::from_learning_signal(0.55, Some(65), 0.15, 0.5, false),
            Some(MutationVerifierSummary::from_report(&verification_report(
                true,
            ))),
            MutationCompensationPolicy {
                bounded: false,
                reversible: true,
                requires_pre_state: false,
                action_category: MutationActionCategory::Execute,
                compensation_kind: Some("git_revert_commit".into()),
                compensation_summary: Some("revert the commit".into()),
            },
        );
        reverted.state = StagedMutationState::Reverted;

        let scoreboard = MutationScoreboard::new("board-1", "session-1", vec![ready, reverted]);
        assert_eq!(scoreboard.total_mutations, 2);
        assert_eq!(scoreboard.ready_mutations, 1);
        assert_eq!(scoreboard.approval_required_mutations, 0);
        assert_eq!(scoreboard.reverted_mutations, 1);
        assert_eq!(scoreboard.blocked_mutations, 0);
        assert!(scoreboard.avg_retention_score.point > 0.5);
        assert!(scoreboard.avg_reward_hacking_risk.point < 0.2);
    }

    #[test]
    fn scoreboard_rehydrates_persisted_decisions() {
        let scoreboard = MutationScoreboard::from_persisted_decisions(
            "board-1",
            "session-1",
            vec![PersistedMutationDecision {
                decision_id: "decision-1".into(),
                session_id: "session-1".into(),
                decision_output: serde_json::json!({
                    "turn": 7,
                    "mutation_objective_score": {
                        "quality": {"point": 0.82, "lower": 0.82, "upper": 0.82},
                        "user_feedback": {"point": 0.91, "lower": 0.91, "upper": 0.91},
                        "reward_hacking_risk": {"point": 0.10, "lower": 0.10, "upper": 0.10},
                        "causal_support": {"point": 0.78, "lower": 0.78, "upper": 0.78},
                        "was_corrected": false
                    },
                    "action_profiles": [
                        {
                            "tool_call_id": "call-1",
                            "tool_name": "write_file",
                            "arguments": {"path": "src/lib.rs"},
                            "verifier_source": "tool_result",
                            "verifier": {
                                "all_required_passed": true,
                                "criteria_total": 2,
                                "criteria_passed": 2,
                                "pass_rate": {"point": 1.0, "lower": 1.0, "upper": 1.0},
                                "failing_criteria": []
                            },
                            "profile": {
                                "bounded": true,
                                "reversible": true,
                                "requires_pre_state": true,
                                "action_category": "write",
                                "compensation_kind": "restore_or_delete_file",
                                "compensation_summary": "restore prior contents"
                            }
                        }
                    ]
                }),
            }],
        );

        assert_eq!(scoreboard.total_mutations, 1);
        assert_eq!(scoreboard.mutations[0].turn_index, 7);
        assert_eq!(scoreboard.mutations[0].tool_name, "write_file");
        assert_eq!(
            scoreboard.mutations[0].tool_args["path"].as_str(),
            Some("src/lib.rs")
        );
        assert_eq!(
            scoreboard.mutations[0].judgment.safety_verdict,
            MutationSafetyVerdict::RequiresApproval
        );
        assert_eq!(
            scoreboard.mutations[0]
                .judgment
                .promotion_verdict
                .recommendation,
            MutationPromotionRecommendation::Canary
        );
        assert_eq!(
            scoreboard.mutations[0]
                .verifier
                .as_ref()
                .map(|summary| summary.criteria_passed),
            Some(2)
        );
        assert_eq!(
            scoreboard.mutations[0].verifier_source.as_deref(),
            Some("tool_result")
        );
        assert_eq!(scoreboard.mutations[0].verifier_gap.as_deref(), None);
        assert_eq!(scoreboard.approval_required_mutations, 1);
    }

    #[test]
    fn promotion_context_downgrades_ready_mutation_on_gate_regression() {
        let scoreboard = MutationScoreboard::new(
            "board-ctx",
            "session-1",
            vec![StagedMutation::new(
                "mut-ctx",
                "session-1",
                1,
                "write_file",
                serde_json::json!({"path": "src/lib.rs"}),
                Some("snap-1".into()),
                MutationObjectiveScore::from_learning_signal(0.93, Some(88), 0.08, 0.82, false),
                Some(MutationVerifierSummary::from_report(&verification_report(
                    true,
                ))),
                automated_policy(true),
            )],
        );

        let scoreboard = scoreboard.with_promotion_context(&MutationPromotionEvaluationContext {
            noise_filtered_quality: Some(ConfidenceInterval::new(0.72, 0.68, 0.76)),
            latest_gate_passed: Some(false),
            latest_gate_score_delta: Some(-0.08),
            latest_gate_score_delta_interval: Some(ValueInterval::new(-0.08, -0.10, -0.04)),
            calibration_error: Some(0.24),
            calibration_error_interval: Some(ValueInterval::new(0.24, 0.20, 0.28)),
            missing_verifier_rate: Some(0.4),
        });

        assert_eq!(scoreboard.ready_mutations, 0);
        assert_eq!(scoreboard.mutations[0].state, StagedMutationState::Pending);
        assert_eq!(
            scoreboard.mutations[0]
                .judgment
                .promotion_verdict
                .recommendation,
            MutationPromotionRecommendation::Canary
        );
        assert!(
            scoreboard.mutations[0]
                .judgment
                .promotion_verdict
                .evidence
                .iter()
                .any(|evidence| evidence == "latest_gate_passed=false")
        );
    }

    #[test]
    fn promotion_context_uses_interval_bounds_for_mutation_penalties() {
        let scoreboard = MutationScoreboard::new(
            "board-intervals",
            "session-1",
            vec![StagedMutation::new(
                "mut-intervals",
                "session-1",
                1,
                "write_file",
                serde_json::json!({"path": "src/lib.rs"}),
                Some("snap-1".into()),
                MutationObjectiveScore::from_learning_signal(0.93, Some(88), 0.08, 0.82, false),
                Some(MutationVerifierSummary::from_report(&verification_report(
                    true,
                ))),
                automated_policy(true),
            )],
        );

        let scoreboard = scoreboard.with_promotion_context(&MutationPromotionEvaluationContext {
            noise_filtered_quality: Some(ConfidenceInterval::new(0.72, 0.68, 0.76)),
            latest_gate_passed: Some(false),
            latest_gate_score_delta: Some(-0.04),
            latest_gate_score_delta_interval: Some(ValueInterval::new(-0.04, -0.18, 0.02)),
            calibration_error: Some(0.18),
            calibration_error_interval: Some(ValueInterval::new(0.18, 0.12, 0.26)),
            missing_verifier_rate: Some(0.1),
        });

        assert!(
            scoreboard.mutations[0]
                .judgment
                .promotion_verdict
                .blockers
                .iter()
                .any(|blocker| blocker == "latest_gate_regression")
        );
        assert!(
            scoreboard.mutations[0]
                .judgment
                .promotion_verdict
                .evidence
                .iter()
                .any(|evidence| evidence.contains("latest_gate_score_delta=-0.04[-0.18,0.02]"))
        );
        assert!(
            scoreboard.mutations[0]
                .judgment
                .promotion_verdict
                .evidence
                .iter()
                .any(|evidence| evidence.contains("calibration_error=0.18[0.12,0.26]"))
        );
    }
}
