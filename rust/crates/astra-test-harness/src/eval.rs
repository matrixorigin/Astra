//! Structured evaluation report: capability scoring, runtime health,
//! and historical comparison.
//!
//! Unlike the text summarizer (which diagnoses problems), this module
//! produces a **structured JSON evaluation** with numeric scores across
//! multiple dimensions. Designed for:
//! - Dashboard visualization (radar charts, trend lines)
//! - Historical comparison (is this release better than the last?)
//! - CI gates (fail the pipeline if any dimension drops below threshold)

use serde::{Deserialize, Serialize};

use crate::criteria::CriterionSeverity;
use crate::report::SuiteReport;

/// Complete structured evaluation of a test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    /// When this evaluation was generated.
    pub evaluated_at: String,
    /// Run metadata.
    pub run_summary: RunSummary,
    /// Per-model capability scores.
    pub model_scores: Vec<ModelScore>,
    /// Astra runtime health assessment.
    pub runtime_health: RuntimeHealth,
    /// Overall composite score (0-100).
    pub overall_score: f64,
    /// Optional LLM-generated narrative summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narrative: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub total_cases: usize,
    pub total_runs: usize,
    pub models_tested: Vec<String>,
    pub wall_time_ms: u64,
    pub pass_rate: f64,
    pub hard_fail_count: usize,
    pub soft_warning_count: usize,
}

/// Scores for a single model across capability dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelScore {
    pub model: String,
    /// Overall score for this model (0-100).
    pub overall: f64,
    /// Per-capability dimension scores.
    pub dimensions: Vec<DimensionScore>,
    /// Efficiency metrics.
    pub efficiency: EfficiencyScore,
}

/// Score for one capability dimension (e.g., tool_use, delegation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionScore {
    pub name: String,
    /// 0-100 score. Based on: pass rate × difficulty weighting × judger scores.
    pub score: f64,
    /// Number of cases in this dimension.
    pub case_count: usize,
    /// Cases that failed in this dimension.
    pub failed_cases: Vec<String>,
}

/// Efficiency metrics for a model (lower is better for tokens/duration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EfficiencyScore {
    /// Average tokens per passed case.
    pub avg_tokens_per_pass: f64,
    /// Average duration (ms) per passed case.
    pub avg_duration_per_pass: f64,
    /// Average LLM round-trips per passed case.
    pub avg_turns_per_pass: f64,
    /// Efficiency score (0-100, higher = more efficient).
    pub score: f64,
}

/// Astra runtime health — NOT model capability, but platform reliability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHealth {
    /// Overall health score (0-100).
    pub score: f64,
    /// Auth stability: did credentials hold for the entire run?
    pub auth_stability: f64,
    /// Infra reliability: rate of InfraTimeout/ProviderError/RateLimit.
    pub infra_reliability: f64,
    /// Execution correctness: rate of non-exit-code failures (runtime bugs).
    pub execution_correctness: f64,
    /// Cases where ALL models failed (suggests runtime/case issue, not model).
    pub universal_failures: Vec<String>,
}

/// Build a structured evaluation from a completed suite report.
pub fn evaluate(report: &SuiteReport) -> EvalReport {
    let models: Vec<String> = {
        let mut m: Vec<String> = report
            .runs
            .iter()
            .map(|r| crate::report::normalize_model_display(&r.model).to_string())
            .collect();
        m.sort();
        m.dedup();
        m
    };

    let run_summary = build_run_summary(report, &models);
    let model_scores: Vec<ModelScore> = models.iter().map(|m| score_model(report, m)).collect();
    let runtime_health = assess_runtime_health(report, &models);

    let overall = if model_scores.is_empty() {
        0.0
    } else {
        let sum: f64 = model_scores.iter().map(|m| m.overall).sum();
        sum / model_scores.len() as f64
    };

    EvalReport {
        evaluated_at: chrono::Utc::now().to_rfc3339(),
        run_summary,
        model_scores,
        runtime_health,
        overall_score: overall,
        narrative: None,
    }
}

fn build_run_summary(report: &SuiteReport, models: &[String]) -> RunSummary {
    let hard_fails = report
        .runs
        .iter()
        .filter(|r| {
            !r.passed
                && r.criteria
                    .iter()
                    .any(|c| c.severity == CriterionSeverity::Hard && !c.passed)
        })
        .count();
    let warnings = report.runs.iter().filter(|r| r.has_warnings).count();

    RunSummary {
        total_cases: {
            let mut names: Vec<&str> = report.runs.iter().map(|r| r.case_name.as_str()).collect();
            names.sort();
            names.dedup();
            names.len()
        },
        total_runs: report.total(),
        models_tested: models.to_vec(),
        wall_time_ms: report.wall_time_ms,
        pass_rate: if report.total() > 0 {
            report.passed() as f64 / report.total() as f64 * 100.0
        } else {
            0.0
        },
        hard_fail_count: hard_fails,
        soft_warning_count: warnings,
    }
}

fn score_model(report: &SuiteReport, model: &str) -> ModelScore {
    let runs: Vec<_> = report
        .runs
        .iter()
        .filter(|r| crate::report::normalize_model_display(&r.model) == model)
        .collect();

    // Group by capability
    let mut cap_groups: std::collections::BTreeMap<String, Vec<&crate::report::CaseRunReport>> =
        std::collections::BTreeMap::new();
    for r in &runs {
        let cap = r
            .capability
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "general".into());
        cap_groups.entry(cap).or_default().push(r);
    }

    let dimensions: Vec<DimensionScore> = cap_groups
        .iter()
        .map(|(cap, cap_runs)| {
            let total = cap_runs.len() as f64;
            let passed = cap_runs.iter().filter(|r| r.passed).count() as f64;

            // Weighted by difficulty: d5 case worth 5x a d1 case.
            let weighted_pass: f64 = cap_runs
                .iter()
                .filter(|r| r.passed)
                .map(|r| r.difficulty.unwrap_or(1) as f64)
                .sum();
            let weighted_total: f64 = cap_runs
                .iter()
                .map(|r| r.difficulty.unwrap_or(1) as f64)
                .sum();

            // Include judger scores for quality.
            let avg_judger: f64 = {
                let scores: Vec<f64> = cap_runs
                    .iter()
                    .flat_map(|r| r.criteria.iter().filter_map(|c| c.score))
                    .collect();
                if scores.is_empty() {
                    1.0
                } else {
                    scores.iter().sum::<f64>() / scores.len() as f64
                }
            };

            let base_rate = if weighted_total > 0.0 {
                weighted_pass / weighted_total
            } else if total > 0.0 {
                passed / total
            } else {
                0.0
            };

            let score = (base_rate * 0.7 + avg_judger * 0.3) * 100.0;

            let failed: Vec<String> = cap_runs
                .iter()
                .filter(|r| !r.passed)
                .map(|r| r.case_name.clone())
                .collect();

            DimensionScore {
                name: cap.clone(),
                score,
                case_count: cap_runs.len(),
                failed_cases: failed,
            }
        })
        .collect();

    let efficiency = compute_efficiency(&runs);

    let dim_avg = if dimensions.is_empty() {
        0.0
    } else {
        dimensions.iter().map(|d| d.score).sum::<f64>() / dimensions.len() as f64
    };
    let overall = dim_avg * 0.7 + efficiency.score * 0.3;

    ModelScore {
        model: model.to_string(),
        overall,
        dimensions,
        efficiency,
    }
}

fn compute_efficiency(runs: &[&crate::report::CaseRunReport]) -> EfficiencyScore {
    let passed: Vec<_> = runs.iter().filter(|r| r.passed).collect();

    // No passes → zero efficiency; there is nothing to measure.
    if passed.is_empty() {
        return EfficiencyScore {
            avg_tokens_per_pass: 0.0,
            avg_duration_per_pass: 0.0,
            avg_turns_per_pass: 0.0,
            score: 0.0,
        };
    }

    let n = passed.len() as f64;

    let avg_tok: f64 = passed
        .iter()
        .map(|r| (r.outcome.prompt_tokens + r.outcome.completion_tokens) as f64)
        .sum::<f64>()
        / n;
    let avg_dur: f64 = passed
        .iter()
        .map(|r| r.outcome.duration_ms as f64)
        .sum::<f64>()
        / n;
    let avg_turns: f64 = passed
        .iter()
        .map(|r| r.outcome.turn_rounds as f64)
        .sum::<f64>()
        / n;

    // Efficiency score: penalize high token/duration usage.
    // Baseline: 10k tokens, 15s, 3 turns = 100 score.
    let tok_score = (1.0 - (avg_tok - 10000.0).max(0.0) / 100000.0).max(0.0) * 100.0;
    let dur_score = (1.0 - (avg_dur - 15000.0).max(0.0) / 300000.0).max(0.0) * 100.0;
    let turn_score = (1.0 - (avg_turns - 3.0).max(0.0) / 20.0).max(0.0) * 100.0;
    let score = tok_score * 0.4 + dur_score * 0.3 + turn_score * 0.3;

    EfficiencyScore {
        avg_tokens_per_pass: avg_tok,
        avg_duration_per_pass: avg_dur,
        avg_turns_per_pass: avg_turns,
        score,
    }
}

fn assess_runtime_health(report: &SuiteReport, models: &[String]) -> RuntimeHealth {
    use crate::classify::FailureClass;

    let total = report.total().max(1) as f64;

    let auth_failures = report
        .runs
        .iter()
        .filter(|r| matches!(r.failure_class, Some(FailureClass::InfraAuth)))
        .count() as f64;
    let infra_failures = report
        .runs
        .iter()
        .filter(|r| {
            matches!(
                r.failure_class,
                Some(
                    FailureClass::InfraTimeout
                        | FailureClass::InfraProviderError { .. }
                        | FailureClass::InfraRateLimit
                )
            )
        })
        .count() as f64;

    // Universal failures: cases where ALL tested models failed.
    let case_names: std::collections::BTreeSet<&str> =
        report.runs.iter().map(|r| r.case_name.as_str()).collect();
    let universal_failures: Vec<String> = case_names
        .iter()
        .filter(|case| {
            let case_runs: Vec<_> = report
                .runs
                .iter()
                .filter(|r| r.case_name == **case)
                .collect();
            case_runs.len() >= models.len().max(1) && case_runs.iter().all(|r| !r.passed)
        })
        .map(|s| s.to_string())
        .collect();

    let auth_stability = (1.0 - auth_failures / total) * 100.0;
    let infra_reliability = (1.0 - infra_failures / total) * 100.0;
    let exec_correctness =
        (1.0 - universal_failures.len() as f64 / case_names.len().max(1) as f64) * 100.0;
    let score = auth_stability * 0.3 + infra_reliability * 0.3 + exec_correctness * 0.4;

    RuntimeHealth {
        score,
        auth_stability,
        infra_reliability,
        execution_correctness: exec_correctness,
        universal_failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{CaseRunReport, SuiteReport};
    use crate::runner::RunOutcome;

    fn mk(case: &str, model: &str, passed: bool, cap: Option<&str>, diff: u8) -> CaseRunReport {
        CaseRunReport {
            case_name: case.into(),
            model: model.into(),
            passed,
            run_index: 0,
            capability: cap.map(|c| match c {
                "tool_use" => crate::case::Capability::ToolUse,
                "delegation" => crate::case::Capability::Delegation,
                "reasoning" => crate::case::Capability::Reasoning,
                _ => crate::case::Capability::Custom(c.into()),
            }),
            weight: 1.0,
            difficulty: Some(diff),
            outcome: {
                let mut o = RunOutcome::new(model);
                o.prompt_tokens = 5000;
                o.completion_tokens = 500;
                o.duration_ms = 10000;
                o.turn_rounds = 2;
                o
            },
            criteria: vec![],
            steps: vec![],
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: None,
            has_warnings: false,
        }
    }

    #[test]
    fn evaluate_produces_structured_scores() {
        let report = SuiteReport {
            runs: vec![
                mk("hello", "A", true, Some("tool_use"), 1),
                mk("hello", "B", true, Some("tool_use"), 1),
                mk("hard", "A", false, Some("delegation"), 4),
                mk("hard", "B", true, Some("delegation"), 4),
            ],
            ..Default::default()
        };
        let eval = evaluate(&report);

        assert_eq!(eval.model_scores.len(), 2);
        assert_eq!(eval.run_summary.total_runs, 4);
        assert_eq!(eval.run_summary.pass_rate, 75.0);

        // B should score higher (2/2 pass vs A's 1/2)
        let a = eval.model_scores.iter().find(|m| m.model == "A").unwrap();
        let b = eval.model_scores.iter().find(|m| m.model == "B").unwrap();
        assert!(
            b.overall > a.overall,
            "B={} should beat A={}",
            b.overall,
            a.overall
        );
    }

    #[test]
    fn runtime_health_detects_universal_failures() {
        let report = SuiteReport {
            runs: vec![
                mk("broken", "A", false, None, 1),
                mk("broken", "B", false, None, 1),
                mk("works", "A", true, None, 1),
                mk("works", "B", true, None, 1),
            ],
            ..Default::default()
        };
        let eval = evaluate(&report);
        assert!(
            eval.runtime_health
                .universal_failures
                .contains(&"broken".to_string()),
            "broken should be a universal failure"
        );
        assert!(eval.runtime_health.score < 100.0);
    }

    #[test]
    fn zero_passes_gives_zero_efficiency() {
        let report = SuiteReport {
            runs: vec![
                mk("fail1", "A", false, Some("tool_use"), 1),
                mk("fail2", "A", false, Some("tool_use"), 2),
            ],
            ..Default::default()
        };
        let eval = evaluate(&report);
        let a = eval.model_scores.iter().find(|m| m.model == "A").unwrap();
        assert_eq!(
            a.efficiency.score, 0.0,
            "efficiency score must be 0 when no cases pass"
        );
        assert_eq!(a.efficiency.avg_tokens_per_pass, 0.0);
        assert_eq!(a.efficiency.avg_duration_per_pass, 0.0);
        assert_eq!(a.efficiency.avg_turns_per_pass, 0.0);
    }

    #[test]
    fn difficulty_weighting_matters() {
        // Model A passes easy (d1) but fails hard (d5).
        // Model B passes both.
        // B should score much higher due to difficulty weighting.
        let report = SuiteReport {
            runs: vec![
                mk("easy", "A", true, Some("reasoning"), 1),
                mk("hard", "A", false, Some("reasoning"), 5),
                mk("easy", "B", true, Some("reasoning"), 1),
                mk("hard", "B", true, Some("reasoning"), 5),
            ],
            ..Default::default()
        };
        let eval = evaluate(&report);
        let a = eval.model_scores.iter().find(|m| m.model == "A").unwrap();
        let b = eval.model_scores.iter().find(|m| m.model == "B").unwrap();
        // A: weighted pass = d1=1, weighted total = d1+d5=6, rate = 1/6 = 16.7%
        // B: weighted pass = d1+d5=6, weighted total = 6, rate = 100%
        assert!(
            b.overall > a.overall * 1.5,
            "B={:.1} should be significantly higher than A={:.1}",
            b.overall,
            a.overall
        );
    }
}
