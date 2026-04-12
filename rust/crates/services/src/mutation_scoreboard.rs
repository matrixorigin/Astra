use astra_core::confidence::ConfidenceInterval;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SubtaskVerificationReport;

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
}

fn is_false(value: &bool) -> bool {
    !*value
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
            .min(ConfidenceInterval::exact(
                1.0 - self.reward_hacking_risk.point,
            ))
            .min(self.causal_support);
        if let Some(feedback) = self.user_feedback {
            score = score.min(feedback);
        }
        if self.was_corrected {
            score = score.min(ConfidenceInterval::exact(0.25));
        }
        score
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
        let criteria_total = report.results.len() as u32;
        let criteria_passed = report.results.iter().filter(|result| result.passed).count() as u32;
        let pass_rate = if criteria_total == 0 {
            ConfidenceInterval::FULL
        } else {
            ConfidenceInterval::exact(criteria_passed as f64 / criteria_total as f64)
        };
        let failing_criteria = report
            .results
            .iter()
            .filter(|result| !result.passed)
            .map(|result| result.criterion_id.clone())
            .collect();
        Self {
            all_required_passed: report.all_required_passed,
            criteria_total,
            criteria_passed,
            pass_rate,
            failing_criteria,
        }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationJudgment {
    pub retention_score: ConfidenceInterval,
    pub verifier_pass_rate: ConfidenceInterval,
    pub safety_verdict: MutationSafetyVerdict,
    pub retention_verdict: MutationRetentionVerdict,
    pub rationale: Vec<String>,
}

impl MutationJudgment {
    pub fn evaluate(
        objective: &MutationObjectiveScore,
        verifier: Option<&MutationVerifierSummary>,
        compensation: &MutationCompensationPolicy,
        has_pre_state_snapshot: bool,
    ) -> Self {
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

        let retention_verdict = if !all_required_passed || retention_score.point < 0.4 {
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
        Self {
            retention_score,
            verifier_pass_rate,
            safety_verdict,
            retention_verdict,
            rationale,
        }
    }

    pub fn staged_state(&self) -> StagedMutationState {
        match (self.retention_verdict, self.safety_verdict) {
            (MutationRetentionVerdict::Reject, _) | (_, MutationSafetyVerdict::Blocked) => {
                StagedMutationState::Blocked
            }
            (MutationRetentionVerdict::Retain, MutationSafetyVerdict::Safe) => {
                StagedMutationState::Ready
            }
            _ => StagedMutationState::Pending,
        }
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
    pub state: StagedMutationState,
    pub objective: MutationObjectiveScore,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<MutationVerifierSummary>,
    pub compensation: MutationCompensationPolicy,
    pub judgment: MutationJudgment,
}

impl StagedMutation {
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
        let judgment = MutationJudgment::evaluate(
            &objective,
            verifier.as_ref(),
            &compensation,
            pre_state_snapshot_id.is_some(),
        );
        let state = judgment.staged_state();
        Self {
            mutation_id: mutation_id.into(),
            session_id: session_id.into(),
            turn_index,
            tool_name: tool_name.into(),
            tool_args,
            pre_state_snapshot_id,
            state,
            objective,
            verifier,
            compensation,
            judgment,
        }
    }
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
                mutation.judgment.safety_verdict == MutationSafetyVerdict::RequiresApproval
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
}

fn normalized_feedback_interval(score: i64) -> ConfidenceInterval {
    ConfidenceInterval::exact((score as f64 / 100.0).clamp(0.0, 1.0))
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
        assert_eq!(scoreboard.approval_required_mutations, 1);
        assert_eq!(scoreboard.reverted_mutations, 1);
        assert_eq!(scoreboard.blocked_mutations, 0);
        assert!(scoreboard.avg_retention_score.point > 0.5);
        assert!(scoreboard.avg_reward_hacking_risk.point < 0.2);
    }
}
