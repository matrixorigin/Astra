//! Bridge between the product capability inventory and model-backed probes.
//!
//! The product matrix owns correctness boundaries. YAML cases own real model
//! journeys. This module prevents the two inventories from silently drifting
//! while preserving the distinction between model behavior and deterministic
//! protocol or isolation proofs.

use std::collections::{BTreeMap, BTreeSet};

use astra_harness::{CAPABILITY_CASES, ModelValidation};

use crate::case::Case;
use crate::criteria::Criterion;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityCoverage {
    pub product_capabilities: usize,
    pub model_probes: Vec<String>,
    pub deterministic_only: usize,
}

/// A model probe must carry at least one machine-checkable assertion about
/// the capability it claims to exercise. Process success, generic terminal
/// state, efficiency bounds, and an LLM judge alone cannot prove product
/// behavior. Keep this match exhaustive so adding a criterion forces an
/// explicit decision about whether it is a product oracle.
fn is_deterministic_product_oracle(criterion: &Criterion) -> bool {
    match criterion {
        Criterion::ExitCode { .. }
        | Criterion::FinalState { .. }
        | Criterion::ToolsCountBetween { .. }
        | Criterion::JournalToolSuccessRatio { .. }
        | Criterion::StderrMatches { .. }
        | Criterion::TextNotContains { .. }
        | Criterion::TokensBetween { .. }
        | Criterion::DurationBetween { .. }
        | Criterion::TurnRoundsBetween { .. }
        | Criterion::SessionSubsystemHealthy { .. }
        | Criterion::Judger { .. }
        | Criterion::HardJudger { .. } => false,

        // Every AnyOf branch must prove product behavior because any one may
        // satisfy the case. An AllOf needs one product oracle because every
        // child is mandatory. This mirrors the criterion's actual Boolean
        // semantics instead of merely searching the syntax tree.
        Criterion::AnyOf { criteria } => {
            !criteria.is_empty() && criteria.iter().all(is_deterministic_product_oracle)
        }
        Criterion::AllOf { criteria } => criteria.iter().any(is_deterministic_product_oracle),

        Criterion::SessionEventCount { optional: true, .. }
        | Criterion::JournalToolCalled { optional: true, .. } => false,

        Criterion::ToolCalled { .. }
        | Criterion::InterruptionKind { .. }
        | Criterion::ToolResultClassCount { .. }
        | Criterion::TextContains { .. }
        | Criterion::TextEquals { .. }
        | Criterion::TextJsonValue { .. }
        | Criterion::TextJsonArrayCount { .. }
        | Criterion::TextJsonPathAbsent { .. }
        | Criterion::TextJsonDag { .. }
        | Criterion::SessionEventCount {
            optional: false, ..
        }
        | Criterion::JournalTurnEvaluationSignalCount { .. }
        | Criterion::JournalToolCalled {
            optional: false, ..
        }
        | Criterion::JournalChildToolCallCount { .. }
        | Criterion::JournalTurnToolHidden { .. }
        | Criterion::JournalToolCallCount { .. }
        | Criterion::JournalToolOutcomeCount { .. }
        | Criterion::JournalToolJson { .. }
        | Criterion::JournalToolJsonContains { .. }
        | Criterion::JournalArtifactConsumed { .. }
        | Criterion::JournalToolValueFlow { .. }
        | Criterion::JournalToolValueFlowBound { .. }
        | Criterion::ForkCacheOutcome { .. }
        | Criterion::ToolSequence { .. }
        | Criterion::JournalToolSequence { .. }
        | Criterion::JournalToolPrecedence { .. }
        | Criterion::JournalWorkItemExecutionFromStart { .. }
        | Criterion::JournalWorkGraphPatch { .. }
        | Criterion::JournalTurnEvaluationSuccess { .. }
        | Criterion::CacheRateAbove { .. }
        | Criterion::PromptCacheTokens { .. }
        | Criterion::ProviderPromptCacheReadRatio { .. }
        | Criterion::ProviderPromptCacheStablePrefixReuseRatio { .. }
        | Criterion::PromptCacheReuseScope { .. }
        | Criterion::PipelineAlertCount { .. }
        | Criterion::PipelineAvgCacheHitRatio { .. } => true,
    }
}

fn case_has_deterministic_product_oracle(case: &Case) -> bool {
    case.criteria
        .iter()
        .chain(case.steps.iter().flat_map(|step| step.criteria.iter()))
        .any(is_deterministic_product_oracle)
}

pub fn validate_capability_coverage(cases: &[Case]) -> Result<CapabilityCoverage, Vec<String>> {
    let mut issues = Vec::new();
    let mut cases_by_name = BTreeMap::new();
    for case in cases {
        if cases_by_name.insert(case.name.as_str(), case).is_some() {
            issues.push(format!(
                "capability probe inventory contains duplicate case name {:?}",
                case.name
            ));
        }
    }
    let mut probes = BTreeSet::new();
    let mut deterministic_only = 0;

    for capability in CAPABILITY_CASES {
        match capability.model_validation {
            ModelValidation::Probe { case } => {
                probes.insert(case.to_string());
                let Some(probe) = cases_by_name.get(case) else {
                    issues.push(format!(
                        "{} references missing model probe {case:?}",
                        capability.id
                    ));
                    continue;
                };
                if probe.prompt.trim().is_empty() {
                    issues.push(format!(
                        "{} model probe {case:?} has an empty prompt",
                        capability.id
                    ));
                }
                if !case_has_deterministic_product_oracle(probe) {
                    issues.push(format!(
                        "{} model probe {case:?} has no deterministic product oracle in its journey; exit status, generic lifecycle/efficiency bounds, and LLM judgement are insufficient",
                        capability.id
                    ));
                }
            }
            ModelValidation::DeterministicOnly { reason } => {
                deterministic_only += 1;
                if reason.trim().is_empty() {
                    issues.push(format!(
                        "{} is deterministic-only without a reason",
                        capability.id
                    ));
                }
            }
        }
    }

    if issues.is_empty() {
        Ok(CapabilityCoverage {
            product_capabilities: CAPABILITY_CASES.len(),
            model_probes: probes.into_iter().collect(),
            deterministic_only,
        })
    } else {
        Err(issues)
    }
}

pub fn retain_model_probe_cases(cases: &mut Vec<Case>) -> Result<CapabilityCoverage, Vec<String>> {
    let coverage = validate_capability_coverage(cases)?;
    let probes: BTreeSet<&str> = coverage.model_probes.iter().map(String::as_str).collect();
    cases.retain(|case| probes.contains(case.name.as_str()));
    Ok(coverage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str) -> Case {
        serde_yaml_ng::from_str(&format!(
            "name: {name}\nprompt: exercise {name}\nmodels: [deepseek-v4-flash]\ncriteria:\n  - type: text_contains\n    needle: evidence\n"
        ))
        .expect("synthetic case")
    }

    #[test]
    fn missing_declared_probe_is_a_coverage_error() {
        let errors = validate_capability_coverage(&[]).expect_err("probes are required");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("missing model probe"))
        );
    }

    #[test]
    fn probe_pack_is_deduplicated_and_only_retains_declared_cases() {
        let probe_names: BTreeSet<&str> = CAPABILITY_CASES
            .iter()
            .filter_map(|capability| match capability.model_validation {
                ModelValidation::Probe { case } => Some(case),
                ModelValidation::DeterministicOnly { .. } => None,
            })
            .collect();
        let mut cases: Vec<Case> = probe_names.iter().map(|name| case(name)).collect();
        cases.push(case("unrelated"));

        let coverage = retain_model_probe_cases(&mut cases).expect("complete probes");

        assert_eq!(cases.len(), probe_names.len());
        assert_eq!(coverage.model_probes.len(), probe_names.len());
        assert!(
            cases
                .iter()
                .all(|case| probe_names.contains(case.name.as_str()))
        );
    }

    #[test]
    fn model_probe_without_machine_checkable_product_evidence_is_rejected() {
        let probe_names: BTreeSet<&str> = CAPABILITY_CASES
            .iter()
            .filter_map(|capability| match capability.model_validation {
                ModelValidation::Probe { case } => Some(case),
                ModelValidation::DeterministicOnly { .. } => None,
            })
            .collect();
        let mut cases: Vec<Case> = probe_names.iter().map(|name| case(name)).collect();
        let weak = cases
            .iter_mut()
            .find(|case| case.name == *probe_names.iter().next().expect("at least one probe"))
            .expect("probe exists");
        weak.criteria = vec![
            Criterion::ExitCode { code: 0 },
            Criterion::ToolsCountBetween { min: 0, max: 3 },
            Criterion::Judger {
                question: "Does it look right?".into(),
                threshold: 0.9,
                model: None,
            },
        ];

        let errors = validate_capability_coverage(&cases).expect_err("weak probe must fail audit");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("no deterministic product oracle"))
        );
    }

    #[test]
    fn optional_or_bypass_cannot_masquerade_as_product_evidence() {
        let probe_names: BTreeSet<&str> = CAPABILITY_CASES
            .iter()
            .filter_map(|capability| match capability.model_validation {
                ModelValidation::Probe { case } => Some(case),
                ModelValidation::DeterministicOnly { .. } => None,
            })
            .collect();
        let mut cases: Vec<Case> = probe_names.iter().map(|name| case(name)).collect();
        let weak = cases
            .iter_mut()
            .find(|case| case.name == *probe_names.iter().next().expect("at least one probe"))
            .expect("probe exists");
        weak.criteria = vec![
            Criterion::AnyOf {
                criteria: vec![
                    Criterion::TextContains {
                        needle: "evidence".into(),
                    },
                    Criterion::ExitCode { code: 0 },
                ],
            },
            Criterion::SessionEventCount {
                event_type: "turn".into(),
                min: 1,
                optional: true,
            },
        ];

        let errors =
            validate_capability_coverage(&cases).expect_err("bypassable evidence must fail audit");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("no deterministic product oracle"))
        );
    }

    #[test]
    fn programmatic_probe_inventory_rejects_duplicate_case_names() {
        let probe_names: BTreeSet<&str> = CAPABILITY_CASES
            .iter()
            .filter_map(|capability| match capability.model_validation {
                ModelValidation::Probe { case } => Some(case),
                ModelValidation::DeterministicOnly { .. } => None,
            })
            .collect();
        let mut cases: Vec<Case> = probe_names.iter().map(|name| case(name)).collect();
        cases.push(case(
            probe_names.iter().next().expect("at least one model probe"),
        ));

        let errors = validate_capability_coverage(&cases).expect_err("duplicates must fail audit");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("duplicate case name"))
        );
    }

    #[test]
    fn multi_turn_probe_can_anchor_product_truth_in_a_follow_up() {
        let probe_names: BTreeSet<&str> = CAPABILITY_CASES
            .iter()
            .filter_map(|capability| match capability.model_validation {
                ModelValidation::Probe { case } => Some(case),
                ModelValidation::DeterministicOnly { .. } => None,
            })
            .collect();
        let mut cases: Vec<Case> = probe_names.iter().map(|name| case(name)).collect();
        let probe = cases
            .iter_mut()
            .find(|case| case.name == *probe_names.iter().next().expect("at least one probe"))
            .expect("probe exists");
        probe.criteria = vec![Criterion::ExitCode { code: 0 }];
        probe.steps.push(crate::case::CaseStep {
            prompt: "follow up".into(),
            criteria: vec![Criterion::TextContains {
                needle: "typed evidence".into(),
            }],
            timeout_seconds: None,
        });

        validate_capability_coverage(&cases)
            .expect("a typed follow-up oracle is part of the complete journey contract");
    }
}
