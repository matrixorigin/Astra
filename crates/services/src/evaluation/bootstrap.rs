//! Pure preparation for starting one frozen evaluation trial.
//!
//! This module owns the boundary between user intent and the canonical Run
//! lifecycle. It derives stable identities and validates the small execution
//! profile supported by the first adapter; it does not create sessions, call
//! providers, or mutate evaluation state.

use super::api::{
    EvaluationExperimentPrepareRequest, EvaluationPrepareTarget, EvaluationTrialStartRequest,
};
use super::durable::{EvaluationExperimentRecord, EvaluationTrialBindingRecord};
use super::execution::{EvaluationRunAdmission, EvaluationSkillRevision};
use super::experiment::{
    DataIsolation, EXPERIMENT_SCHEMA_VERSION, EvaluationBudget, EvaluationCase,
    EvaluationJudgmentPolicy, EvaluationTarget, EvaluationTargetKind, ExperimentSpec,
    FrozenConditions, FrozenSkillRoutingPolicy, FrozenWorkspaceExecution, MemoryIsolation,
    RevisionRef, TrialOrder,
};
use super::{
    EvaluationPolicyFingerprintInput, content_fingerprint, evaluation_policy_fingerprint,
    is_no_skill_revision, prompt_context_fingerprint,
};
use astra_core::canonical_json_string;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_SKILL_NAME_BYTES: usize = 128;
const MAX_CONCURRENCY: u16 = 64;
const MAX_WALL_TIME_SECS: u64 = 24 * 60 * 60;
pub const EVALUATION_ADAPTER_PROFILE_VERSION: &str = "prompt-skill-text.v1";

/// Stable, redacted identity for the model-side prompt-cache behavior frozen
/// into an evaluation plan. The first adapter deliberately exposes only the
/// two cache contracts that the canonical Run preflight can prove; it does not
/// pretend to compare provider-specific internals.
pub fn prepared_cache_policy_identity(
    _provider: &str,
    cache_capability: Option<&crate::models::PromptCacheCapabilityData>,
) -> String {
    if cache_capability.is_some() {
        "explicit_recorded".to_string()
    } else {
        "provider_default_recorded".to_string()
    }
}

/// Derive the owner-scoped experiment address from the idempotent submission
/// key. The key remains the durable semantic idempotency boundary; this
/// derived address keeps clients from choosing arbitrary cross-owner ids.
pub fn prepared_experiment_id(
    owner_user_id: &str,
    submission_idempotency_key: &str,
) -> Result<String, EvaluationBootstrapError> {
    if owner_user_id.trim().is_empty() || submission_idempotency_key.trim().is_empty() {
        return Err(EvaluationBootstrapError::InvalidInput(
            "owner_user_id and submission_idempotency_key must not be empty".to_string(),
        ));
    }
    if owner_user_id.len() > 128 || submission_idempotency_key.len() > 128 {
        return Err(EvaluationBootstrapError::InvalidInput(
            "owner_user_id and submission_idempotency_key are too long".to_string(),
        ));
    }
    let payload = json!({
        "schema_version": 1,
        "owner_user_id": owner_user_id,
        "submission_idempotency_key": submission_idempotency_key,
    });
    let digest = Sha256::digest(canonical_json_string(&payload).as_bytes());
    Ok(format!("evx_{digest:x}"))
}

/// Compare a retry's user intent with the already-frozen material without
/// re-resolving mutable model or Skill catalogs. This is what makes the
/// submission key a real idempotency boundary: an identical retry can replay
/// the original plan after a catalog change, while a different request gets a
/// conflict instead of silently adopting new facts.
pub fn prepared_request_matches_spec(
    request: &EvaluationExperimentPrepareRequest,
    spec: &ExperimentSpec,
) -> bool {
    if request.model_offering_id != spec.conditions.model_binding
        || request.max_concurrency != spec.budget.max_concurrency
        || request.max_wall_time_secs != spec.budget.max_wall_time_secs
        || spec.adapter_profile_version.as_deref() != Some(EVALUATION_ADAPTER_PROFILE_VERSION)
        || spec.cases.len() != 1
    {
        return false;
    }
    if !workspace_intent_matches(request, spec.conditions.workspace_execution.as_ref()) {
        return false;
    }
    let case = &spec.cases[0];
    if case.case_id != request.case.case_id
        || case.holdout != request.case.holdout
        || case.input_content.as_deref() != Some(request.case.message.as_str())
        || case.task_verifier.config != request.case.verifier_config
    {
        return false;
    }
    match &request.target.kind {
        EvaluationTargetKind::Prompt => {
            if spec.target.kind != EvaluationTargetKind::Prompt
                || request.target.skill_name.is_some()
                || request.judgment_model_offering_id.is_some()
                || spec.target.skill_name.is_some()
                || !matches!(
                    &spec.target.judgment_policy,
                    EvaluationJudgmentPolicy::Disabled
                )
                || request.target.baseline.content.as_deref()
                    != spec.target.baseline.content.as_deref()
                || request.target.candidate.content.as_deref()
                    != spec.target.candidate.content.as_deref()
            {
                return false;
            }
        }
        EvaluationTargetKind::Skill => {
            if spec.target.kind != EvaluationTargetKind::Skill
                || request.target.baseline.content.is_some()
                || request.target.candidate.content.is_some()
                || request.judgment_model_offering_id.is_some()
                || request.target.skill_name.as_deref() != spec.target.skill_name.as_deref()
                || !matches!(
                    &spec.target.judgment_policy,
                    EvaluationJudgmentPolicy::Disabled
                )
            {
                return false;
            }
        }
        EvaluationTargetKind::SkillRoutingJudgment => {
            let offering_matches = match (
                request.judgment_model_offering_id.as_deref(),
                &spec.target.judgment_policy,
            ) {
                (Some(requested), EvaluationJudgmentPolicy::SkillRouting { candidate }) => {
                    matches!(
                        candidate.as_ref(),
                        FrozenSkillRoutingPolicy::Available { model, .. }
                            if model.offering_id == requested
                    )
                }
                (None, EvaluationJudgmentPolicy::SkillRouting { .. }) => true,
                _ => false,
            };
            if spec.target.kind != EvaluationTargetKind::SkillRoutingJudgment
                || request.target.baseline.content.is_some()
                || request.target.candidate.content.is_some()
                || request.target.skill_name.as_deref() != spec.target.skill_name.as_deref()
                || request.target.baseline.revision_id != request.target.candidate.revision_id
                || !offering_matches
            {
                return false;
            }
        }
        _ => return false,
    }
    request.target.baseline.revision_id == spec.target.baseline.revision_id
        && request.target.candidate.revision_id == spec.target.candidate.revision_id
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSkillIdentity {
    pub skill_name: String,
    pub baseline_revision_id: String,
    pub baseline_content_hash: String,
    pub candidate_revision_id: String,
    pub candidate_content_hash: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EvaluationBootstrapError {
    #[error("invalid evaluation start request: {0}")]
    InvalidInput(String),
    #[error("evaluation start conflicts with the frozen plan: {0}")]
    Conflict(String),
    #[error("evaluation target is not executable by the current adapter: {0}")]
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluationTrialStartPlan {
    pub experiment_id: String,
    pub trial_id: String,
    pub session_id: String,
    pub run_id: String,
    pub request_fingerprint: String,
    pub message: String,
    pub revision_content: Option<String>,
    pub model_offering_id: String,
    pub execution_time_budget_secs: u64,
    /// The frozen Edge identity used by the canonical Run binding owner. The
    /// plan never carries a workspace path or materialization identity.
    pub edge_executor_id: Option<String>,
    pub workspace_execution: Option<FrozenWorkspaceExecution>,
    pub admission: EvaluationRunAdmission,
}

fn workspace_intent_matches(
    request: &EvaluationExperimentPrepareRequest,
    prepared: Option<&FrozenWorkspaceExecution>,
) -> bool {
    match (request.workspace.as_ref(), prepared) {
        (None, None) => true,
        (Some(intent), Some(frozen)) => {
            let mut tools = intent.tool_names.clone();
            tools.sort();
            intent.edge_executor_id == frozen.edge_executor_id
                && intent.source_commit.to_ascii_lowercase() == frozen.source_commit
                && tools == frozen.tool_names
        }
        _ => false,
    }
}

fn prepared_workspace(
    request: &EvaluationExperimentPrepareRequest,
    prepared: Option<&FrozenWorkspaceExecution>,
) -> Result<Option<FrozenWorkspaceExecution>, EvaluationBootstrapError> {
    let prepared = prepared
        .cloned()
        .map(FrozenWorkspaceExecution::normalized)
        .transpose()
        .map_err(EvaluationBootstrapError::InvalidInput)?;
    if !workspace_intent_matches(request, prepared.as_ref()) {
        return Err(EvaluationBootstrapError::Conflict(
            "workspace intent requires matching server-prepared confinement capability".into(),
        ));
    }
    Ok(prepared)
}

fn judgment_policy_for_trial(
    target: &EvaluationTarget,
    arm: &super::ComparisonArm,
) -> Option<FrozenSkillRoutingPolicy> {
    match (&target.judgment_policy, arm) {
        (EvaluationJudgmentPolicy::SkillRouting { candidate }, super::ComparisonArm::Candidate) => {
            Some(candidate.as_ref().clone())
        }
        _ => None,
    }
}

/// Convert authenticated user intent plus trusted model/Skill/workspace facts into the
/// immutable first adapter spec. Hashes and policy identities are produced
/// here, never by a client or a second runtime implementation.
pub fn build_prepared_experiment_spec(
    workspace: Option<&FrozenWorkspaceExecution>,
    experiment_id: &str,
    request: &EvaluationExperimentPrepareRequest,
    execution_config: &super::execution_config::EvaluationExecutionConfig,
    skill: Option<&PreparedSkillIdentity>,
    judgment_policy: EvaluationJudgmentPolicy,
) -> Result<ExperimentSpec, EvaluationBootstrapError> {
    if experiment_id.trim().is_empty() {
        return Err(EvaluationBootstrapError::InvalidInput(
            "experiment_id must not be empty".to_string(),
        ));
    }
    if request.case.message.trim().is_empty() {
        return Err(EvaluationBootstrapError::InvalidInput(
            "case.message must not be empty".to_string(),
        ));
    }
    if request.case.message.len() > MAX_MESSAGE_BYTES {
        return Err(EvaluationBootstrapError::InvalidInput(format!(
            "case.message exceeds the {MAX_MESSAGE_BYTES} byte limit"
        )));
    }
    if request.target.kind != EvaluationTargetKind::SkillRoutingJudgment
        && request.judgment_model_offering_id.is_some()
    {
        return Err(EvaluationBootstrapError::InvalidInput(
            "judgment_model_offering_id is only valid for SkillRoutingJudgment targets".to_string(),
        ));
    }
    if request.max_concurrency == 0 || request.max_concurrency > MAX_CONCURRENCY {
        return Err(EvaluationBootstrapError::InvalidInput(format!(
            "max_concurrency must be between 1 and {MAX_CONCURRENCY}"
        )));
    }
    if request.max_wall_time_secs == 0 || request.max_wall_time_secs > MAX_WALL_TIME_SECS {
        return Err(EvaluationBootstrapError::InvalidInput(format!(
            "max_wall_time_secs must be between 1 and {MAX_WALL_TIME_SECS}"
        )));
    }
    execution_config
        .validate()
        .map_err(EvaluationBootstrapError::InvalidInput)?;
    let model = &execution_config.model;
    let cache_policy =
        prepared_cache_policy_identity(&model.provider, model.cache_capability.as_ref());
    if model.offering_id != request.model_offering_id || model.provider.trim().is_empty() {
        return Err(EvaluationBootstrapError::Conflict(
            "model admission does not match the requested offering".to_string(),
        ));
    }
    let input_content_hash = prompt_context_fingerprint(&request.case.message, &[], &[], None);
    let target = build_prepared_target(
        &request.target,
        input_content_hash.as_str(),
        skill,
        judgment_policy,
    )?;
    let resolved_model_selection = crate::runs::ResolvedModelSelection {
        offering_id: model.offering_id.clone(),
        model_name: model.model_name.clone(),
    };
    let workspace_execution = prepared_workspace(request, workspace)?;
    let execution_policy = crate::runs::ExecutionPolicyRequest::default();
    let tool_policy_hash = evaluation_policy_fingerprint(&EvaluationPolicyFingerprintInput {
        model_binding: &model.offering_id,
        provider_binding: &model.provider,
        cache_policy: &cache_policy,
        resolved_model_selection: Some(&resolved_model_selection),
        admitted_provider: &model.provider,
        admitted_cache_capability: model.cache_capability.as_ref(),
        execution_policy: &execution_policy,
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: workspace_execution
            .as_ref()
            .map(|workspace| workspace.tool_names.as_slice()),
        enabled_tools: None,
        runtime_profile: None,
        workspace_execution: workspace_execution.as_ref(),
    })
    .map_err(EvaluationBootstrapError::InvalidInput)?;
    let spec = ExperimentSpec {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        experiment_id: experiment_id.to_string(),
        target,
        cases: vec![EvaluationCase {
            case_id: request.case.case_id.clone(),
            input_snapshot_ref: format!("evaluation://input/{input_content_hash}"),
            input_content_hash: input_content_hash.clone(),
            holdout: request.case.holdout,
            task_verifier: super::task_verifier::TaskVerifierSpec::freeze(
                request.case.verifier_config.clone(),
            )
            .map_err(EvaluationBootstrapError::InvalidInput)?,
            input_content: Some(request.case.message.clone()),
        }],
        repetitions: 1,
        order: TrialOrder::BaselineFirst,
        conditions: FrozenConditions {
            execution_config: execution_config.clone(),
            isolation_profile: if workspace_execution.is_some() {
                "edge_workspace_private_v1".to_string()
            } else {
                "prompt_only_private".to_string()
            },
            model_binding: model.offering_id.clone(),
            provider_binding: model.provider.clone(),
            context_snapshot_hash: input_content_hash,
            tool_policy_hash,
            cache_policy,
            memory_isolation: MemoryIsolation::Disabled,
            data_isolation: DataIsolation::Disabled,
            workspace_execution,
        },
        budget: EvaluationBudget {
            max_trials: 2,
            max_concurrency: request.max_concurrency,
            max_wall_time_secs: request.max_wall_time_secs,
        },
        adapter_profile_version: Some(EVALUATION_ADAPTER_PROFILE_VERSION.to_string()),
        measurement_profile:
            crate::evaluation::measurement_profile::MeasurementProfile::InstructionOnlyV1,
    };
    spec.validate_for_current_execution()
        .map_err(EvaluationBootstrapError::InvalidInput)?;
    Ok(spec)
}

fn build_prepared_target(
    target: &EvaluationPrepareTarget,
    _input_content_hash: &str,
    skill: Option<&PreparedSkillIdentity>,
    judgment_policy: EvaluationJudgmentPolicy,
) -> Result<EvaluationTarget, EvaluationBootstrapError> {
    match &target.kind {
        EvaluationTargetKind::Prompt => {
            if target.skill_name.is_some() || skill.is_some() {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Prompt preparation does not accept skill_name".to_string(),
                ));
            }
            let baseline = target.baseline.content.clone().ok_or_else(|| {
                EvaluationBootstrapError::InvalidInput(
                    "Prompt baseline content is required".to_string(),
                )
            })?;
            let candidate = target.candidate.content.clone().ok_or_else(|| {
                EvaluationBootstrapError::InvalidInput(
                    "Prompt candidate content is required".to_string(),
                )
            })?;
            Ok(EvaluationTarget {
                kind: EvaluationTargetKind::Prompt,
                baseline: RevisionRef {
                    revision_id: target.baseline.revision_id.clone(),
                    content_hash: content_fingerprint(&baseline),
                    content: Some(baseline),
                },
                candidate: RevisionRef {
                    revision_id: target.candidate.revision_id.clone(),
                    content_hash: content_fingerprint(&candidate),
                    content: Some(candidate),
                },
                skill_name: None,
                judgment_policy,
            })
        }
        EvaluationTargetKind::Skill | EvaluationTargetKind::SkillRoutingJudgment => {
            if target.baseline.content.is_some() || target.candidate.content.is_some() {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Skill preparation uses owner-scoped revision IDs, not inline content"
                        .to_string(),
                ));
            }
            let Some(skill) = skill else {
                return Err(EvaluationBootstrapError::Conflict(
                    "owner-scoped Skill revisions could not be resolved".to_string(),
                ));
            };
            if target.skill_name.as_deref() != Some(skill.skill_name.as_str())
                || target.baseline.revision_id != skill.baseline_revision_id
                || target.candidate.revision_id != skill.candidate_revision_id
            {
                return Err(EvaluationBootstrapError::Conflict(
                    "resolved Skill revisions do not match the requested identity".to_string(),
                ));
            }
            if target.kind == EvaluationTargetKind::Skill
                && !matches!(judgment_policy, EvaluationJudgmentPolicy::Disabled)
            {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Skill targets require a disabled judgment policy".to_string(),
                ));
            }
            if target.kind == EvaluationTargetKind::SkillRoutingJudgment
                && !matches!(
                    judgment_policy,
                    EvaluationJudgmentPolicy::SkillRouting { .. }
                )
            {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "SkillRoutingJudgment targets require a candidate judgment policy".to_string(),
                ));
            }
            Ok(EvaluationTarget {
                kind: target.kind.clone(),
                baseline: RevisionRef {
                    revision_id: skill.baseline_revision_id.clone(),
                    content_hash: skill.baseline_content_hash.clone(),
                    content: None,
                },
                candidate: RevisionRef {
                    revision_id: skill.candidate_revision_id.clone(),
                    content_hash: skill.candidate_content_hash.clone(),
                    content: None,
                },
                skill_name: Some(skill.skill_name.clone()),
                judgment_policy,
            })
        }
        other => Err(EvaluationBootstrapError::Unsupported(format!(
            "target kind {other:?} is not supported by the current prepare adapter"
        ))),
    }
}

/// Validate the frozen trial and derive every server-owned identity needed by
/// the canonical Run entrypoint. The first execution adapter intentionally
/// accepts only a fixed text case with no extra request context.
pub fn prepare_trial_start(
    owner_user_id: &str,
    experiment: &EvaluationExperimentRecord,
    trial: &EvaluationTrialBindingRecord,
    request: &EvaluationTrialStartRequest,
) -> Result<EvaluationTrialStartPlan, EvaluationBootstrapError> {
    if owner_user_id.trim().is_empty() {
        return Err(EvaluationBootstrapError::InvalidInput(
            "owner_user_id must not be empty".to_string(),
        ));
    }
    let edge_executor_id = request
        .edge_executor_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if request.edge_executor_id.is_some() && edge_executor_id.is_none() {
        return Err(EvaluationBootstrapError::InvalidInput(
            "edge_executor_id must not be empty".to_string(),
        ));
    }
    let workspace_execution = experiment.spec.conditions.workspace_execution.clone();
    let edge_executor_id = match workspace_execution.as_ref() {
        Some(workspace) => {
            if let Some(requested) = edge_executor_id.as_deref()
                && requested != workspace.edge_executor_id
            {
                return Err(EvaluationBootstrapError::Conflict(
                    "edge_executor_id does not match the frozen workspace execution".to_string(),
                ));
            }
            Some(workspace.edge_executor_id.clone())
        }
        None if edge_executor_id.is_some() => {
            return Err(EvaluationBootstrapError::Unsupported(
                "Edge evaluation requires a frozen workspace execution policy".to_string(),
            ));
        }
        None => None,
    };
    if experiment.owner_user_id != owner_user_id || trial.owner_user_id != owner_user_id {
        return Err(EvaluationBootstrapError::Conflict(
            "evaluation plan is owned by another user".to_string(),
        ));
    }
    if experiment.experiment_id != trial.experiment_id {
        return Err(EvaluationBootstrapError::Conflict(
            "trial does not belong to the requested experiment".to_string(),
        ));
    }
    if trial.binding_status != "planned" && trial.binding_status != "bound" {
        return Err(EvaluationBootstrapError::Conflict(format!(
            "trial has unsupported binding status {}",
            trial.binding_status
        )));
    }
    experiment
        .spec
        .validate_trial_identity(&trial.trial)
        .map_err(EvaluationBootstrapError::Conflict)?;
    let case = experiment
        .spec
        .cases
        .iter()
        .find(|case| case.case_id == trial.trial.case_id)
        .ok_or_else(|| {
            EvaluationBootstrapError::Conflict(format!(
                "trial case `{}` is missing from the frozen experiment",
                trial.trial.case_id
            ))
        })?;
    let message = match (&request.message, case.input_content.as_ref()) {
        (Some(requested), Some(frozen)) if requested != frozen => {
            return Err(EvaluationBootstrapError::Conflict(
                "message does not match the frozen case input".to_string(),
            ));
        }
        (Some(requested), _) => requested.clone(),
        (None, Some(frozen)) => frozen.clone(),
        (None, None) => {
            return Err(EvaluationBootstrapError::InvalidInput(
                "message is required when the frozen case has no input content".to_string(),
            ));
        }
    };
    if message.trim().is_empty() {
        return Err(EvaluationBootstrapError::InvalidInput(
            "message must not be empty".to_string(),
        ));
    }
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(EvaluationBootstrapError::InvalidInput(format!(
            "message exceeds the {MAX_MESSAGE_BYTES} byte limit"
        )));
    }

    // The initial adapter freezes the full Context assembly to exactly this
    // text payload. A caller cannot smuggle an unrecorded context through a
    // start request and still receive an Available observation.
    let input_content_hash = prompt_context_fingerprint(&message, &[], &[], None);
    if input_content_hash != trial.trial.input_content_hash
        || input_content_hash != experiment.spec.conditions.context_snapshot_hash
    {
        return Err(EvaluationBootstrapError::Conflict(
            "message does not match the frozen trial input identity".to_string(),
        ));
    }

    let revision = match trial.trial.arm {
        super::ComparisonArm::Baseline => &experiment.spec.target.baseline,
        super::ComparisonArm::Candidate => &experiment.spec.target.candidate,
    };
    let (revision_content, skill_revision) = match &experiment.spec.target.kind {
        EvaluationTargetKind::Prompt => {
            if request.skill_name.is_some() {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Prompt evaluation does not accept skill_name".to_string(),
                ));
            }
            let content = match (&request.revision_content, revision.content.as_ref()) {
                (Some(requested), Some(frozen)) if requested != frozen => {
                    return Err(EvaluationBootstrapError::Conflict(
                        "revision_content does not match the frozen revision".to_string(),
                    ));
                }
                (Some(requested), _) => requested.clone(),
                (None, Some(frozen)) => frozen.clone(),
                (None, None) => {
                    return Err(EvaluationBootstrapError::InvalidInput(
                        "Prompt evaluation requires revision_content or a frozen revision content"
                            .to_string(),
                    ));
                }
            };
            if content_fingerprint(&content) != revision.content_hash {
                return Err(EvaluationBootstrapError::Conflict(
                    "revision_content does not match the frozen revision".to_string(),
                ));
            }
            (Some(content), None)
        }
        EvaluationTargetKind::Skill | EvaluationTargetKind::SkillRoutingJudgment => {
            if request.revision_content.is_some() {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Skill evaluation does not accept revision_content".to_string(),
                ));
            }
            let skill_name = match (
                &request.skill_name,
                experiment.spec.target.skill_name.as_ref(),
            ) {
                (Some(requested), Some(frozen)) if requested != frozen => {
                    return Err(EvaluationBootstrapError::Conflict(
                        "skill_name does not match the frozen target".to_string(),
                    ));
                }
                (Some(requested), _) => requested.clone(),
                (None, Some(frozen)) => frozen.clone(),
                (None, None) => {
                    return Err(EvaluationBootstrapError::InvalidInput(
                        "Skill evaluation requires skill_name or a frozen target name".to_string(),
                    ));
                }
            };
            if skill_name.trim().is_empty() || skill_name.len() > MAX_SKILL_NAME_BYTES {
                return Err(EvaluationBootstrapError::InvalidInput(format!(
                    "skill_name must be non-empty and at most {MAX_SKILL_NAME_BYTES} bytes"
                )));
            }
            if is_no_skill_revision(&revision.revision_id, &revision.content_hash) {
                (None, None)
            } else {
                let skill_revision = EvaluationSkillRevision {
                    skill_name,
                    revision_id: revision.revision_id.clone(),
                    content_hash: revision.content_hash.clone(),
                };
                skill_revision
                    .validate_shape()
                    .map_err(EvaluationBootstrapError::InvalidInput)?;
                (None, Some(skill_revision))
            }
        }
        other => {
            return Err(EvaluationBootstrapError::Unsupported(format!(
                "target kind {other:?}"
            )));
        }
    };

    if request
        .execution_time_budget_secs
        .is_some_and(|value| value != experiment.spec.budget.max_wall_time_secs)
    {
        return Err(EvaluationBootstrapError::Conflict(
            "execution_time_budget_secs differs from the frozen wall-time budget".to_string(),
        ));
    }
    let execution_time_budget_secs = experiment.spec.budget.max_wall_time_secs;
    if execution_time_budget_secs == 0 {
        return Err(EvaluationBootstrapError::Conflict(
            "the frozen wall-time budget must be positive".to_string(),
        ));
    }

    let identity_payload = json!({
        "schema_version": 1,
        "owner_user_id": owner_user_id,
        "experiment_id": experiment.experiment_id,
        "trial_id": trial.trial_id,
    });
    let identity_canonical = canonical_json_string(&identity_payload);
    let identity_digest = format!("{:x}", Sha256::digest(identity_canonical.as_bytes()));
    let session_id = format!("evs_{}", &identity_digest[..60]);
    let run_id = format!("evr_{}", &identity_digest[4..64]);

    match (
        trial.binding_status.as_str(),
        trial.session_id.as_deref(),
        trial.run_id.as_deref(),
    ) {
        ("planned", None, None) => {}
        ("bound", Some(bound_session), Some(bound_run))
            if bound_session == session_id && bound_run == run_id => {}
        ("planned", Some(_), _) | ("planned", _, Some(_)) => {
            return Err(EvaluationBootstrapError::Conflict(
                "planned trial has a partial canonical Run binding".to_string(),
            ));
        }
        ("bound", _, _) => {
            return Err(EvaluationBootstrapError::Conflict(
                "trial is bound to a different canonical session or Run".to_string(),
            ));
        }
        (status, _, _) => {
            return Err(EvaluationBootstrapError::Conflict(format!(
                "trial has unsupported binding status {status}"
            )));
        }
    }

    // Normalize the default budget before hashing so omitted and explicit
    // frozen-budget requests are the same logical start intent.
    let request_payload = json!({
        "schema_version": 1,
        "owner_user_id": owner_user_id,
        "experiment_id": experiment.experiment_id,
        "trial_id": trial.trial_id,
        "message": message,
        "revision_content": revision_content,
        "skill_name": skill_revision.as_ref().map(|revision| &revision.skill_name),
        "execution_time_budget_secs": execution_time_budget_secs,
        "edge_executor_id": edge_executor_id,
        "workspace_execution": workspace_execution,
        "judgment_policy": judgment_policy_for_trial(&experiment.spec.target, &trial.trial.arm),
    });
    let request_fingerprint = format!(
        "{:x}",
        Sha256::digest(canonical_json_string(&request_payload).as_bytes())
    );
    let admission = EvaluationRunAdmission {
        experiment_id: experiment.experiment_id.clone(),
        trial_id: trial.trial_id.clone(),
        input_content_hash: trial.trial.input_content_hash.clone(),
        revision_content_hash: revision.content_hash.clone(),
        skill_revision,
        judgment_policy: judgment_policy_for_trial(&experiment.spec.target, &trial.trial.arm),
        receipt_ids: Vec::new(),
        snapshot_envelope: None,
    };
    Ok(EvaluationTrialStartPlan {
        experiment_id: experiment.experiment_id.clone(),
        trial_id: trial.trial_id.clone(),
        session_id,
        run_id,
        request_fingerprint,
        message,
        revision_content,
        model_offering_id: experiment.spec.conditions.model_binding.clone(),
        execution_time_budget_secs,
        edge_executor_id,
        workspace_execution,
        admission,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::assessment::ComparisonArm;
    use crate::evaluation::experiment::{
        DataIsolation, EvaluationBudget, EvaluationCase, EvaluationTarget, ExperimentSpec,
        FrozenConditions, MemoryIsolation, RevisionRef, TrialOrder,
    };
    use crate::evaluation::{NO_SKILL_CONTENT_HASH, NO_SKILL_REVISION_ID};

    fn fixtures() -> (
        EvaluationExperimentRecord,
        EvaluationTrialBindingRecord,
        String,
    ) {
        let message = "fixed input".to_string();
        let input_hash = prompt_context_fingerprint(&message, &[], &[], None);
        let baseline = "baseline prompt".to_string();
        let candidate = "candidate prompt".to_string();
        let spec = ExperimentSpec {
            schema_version: super::super::experiment::EXPERIMENT_SCHEMA_VERSION,
            experiment_id: "exp-bootstrap".to_string(),
            target: EvaluationTarget {
                kind: EvaluationTargetKind::Prompt,
                baseline: RevisionRef {
                    revision_id: "base".to_string(),
                    content_hash: content_fingerprint(&baseline),
                    content: Some(baseline.clone()),
                },
                candidate: RevisionRef {
                    revision_id: "cand".to_string(),
                    content_hash: content_fingerprint(&candidate),
                    content: Some(candidate.clone()),
                },
                skill_name: None,
                judgment_policy: EvaluationJudgmentPolicy::Disabled,
            },
            cases: vec![EvaluationCase {
                case_id: "case-1".to_string(),
                input_snapshot_ref: "input://case-1".to_string(),
                input_content_hash: input_hash.clone(),
                holdout: false,
                input_content: Some(message.clone()),
                task_verifier: crate::evaluation::task_verifier::TaskVerifierSpec::freeze(
                    crate::evaluation::task_verifier::JsonValueEqualsConfig {
                        expected: serde_json::json!({"ok": true}),
                    },
                )
                .unwrap(),
            }],
            repetitions: 1,
            order: TrialOrder::BaselineFirst,
            conditions: FrozenConditions {
                execution_config: crate::evaluation::test_support::execution_config(
                    "model-1",
                    "provider-1",
                    "case-1",
                ),
                isolation_profile: "prompt_only_private".to_string(),
                model_binding: "model-1".to_string(),
                provider_binding: "provider-1".to_string(),
                context_snapshot_hash: input_hash,
                tool_policy_hash: "sha256:policy".to_string(),
                cache_policy: "provider_default_recorded".to_string(),
                memory_isolation: MemoryIsolation::Disabled,
                data_isolation: DataIsolation::Disabled,
                workspace_execution: None,
            },
            budget: EvaluationBudget {
                max_trials: 2,
                max_concurrency: 1,
                max_wall_time_secs: 30,
            },
            adapter_profile_version: None,
            measurement_profile:
                crate::evaluation::measurement_profile::MeasurementProfile::InstructionOnlyV1,
        };
        let spec_fingerprint = spec.spec_fingerprint().expect("fingerprint");
        let planned = spec.plan_trials().expect("trials");
        let trial = planned
            .into_iter()
            .find(|trial| trial.arm == ComparisonArm::Baseline)
            .expect("baseline");
        let binding = EvaluationTrialBindingRecord {
            owner_user_id: "owner-1".to_string(),
            trial_id: trial.trial_id.clone(),
            experiment_id: spec.experiment_id.clone(),
            spec_fingerprint,
            trial,
            binding_status: "planned".to_string(),
            session_id: None,
            run_id: None,
            run_generation: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        };
        let record = EvaluationExperimentRecord {
            owner_user_id: "owner-1".to_string(),
            experiment_id: spec.experiment_id.clone(),
            spec_fingerprint: spec.spec_fingerprint().expect("fingerprint"),
            spec,
            submission_idempotency_key: "submit-1".to_string(),
            planned_trial_count: 2,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        };
        (record, binding, message)
    }

    #[test]
    fn derives_stable_session_run_and_normalized_request_identity() {
        let (experiment, trial, message) = fixtures();
        let request = EvaluationTrialStartRequest {
            message: None,
            revision_content: None,
            skill_name: None,
            execution_time_budget_secs: None,
            edge_executor_id: None,
        };
        let explicit = EvaluationTrialStartRequest {
            execution_time_budget_secs: Some(30),
            ..request.clone()
        };
        let first = prepare_trial_start("owner-1", &experiment, &trial, &request).expect("plan");
        let second = prepare_trial_start("owner-1", &experiment, &trial, &explicit).expect("plan");
        assert_eq!(first.session_id, second.session_id);
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(first.request_fingerprint, second.request_fingerprint);
        assert!(first.session_id.starts_with("evs_"));
        assert!(first.run_id.starts_with("evr_"));
        assert_eq!(first.message, message);
        assert_eq!(first.revision_content.as_deref(), Some("baseline prompt"));
    }

    #[test]
    fn rejects_context_or_revision_drift_before_execution() {
        let (experiment, trial, _) = fixtures();
        let mut request = EvaluationTrialStartRequest {
            message: Some("changed".to_string()),
            revision_content: None,
            skill_name: None,
            execution_time_budget_secs: None,
            edge_executor_id: None,
        };
        assert!(matches!(
            prepare_trial_start("owner-1", &experiment, &trial, &request),
            Err(EvaluationBootstrapError::Conflict(_))
        ));
        request.message = Some("fixed input".to_string());
        request.revision_content = Some("changed prompt".to_string());
        assert!(matches!(
            prepare_trial_start("owner-1", &experiment, &trial, &request),
            Err(EvaluationBootstrapError::Conflict(_))
        ));
    }

    #[test]
    fn rejects_partial_or_foreign_existing_binding() {
        let (experiment, mut trial, message) = fixtures();
        let request = EvaluationTrialStartRequest {
            message: Some(message),
            revision_content: None,
            skill_name: None,
            execution_time_budget_secs: None,
            edge_executor_id: None,
        };
        trial.session_id = Some("evs_partial".to_string());
        let error = prepare_trial_start("owner-1", &experiment, &trial, &request)
            .expect_err("partial binding must not be reused");
        assert!(matches!(error, EvaluationBootstrapError::Conflict(_)));

        trial.binding_status = "bound".to_string();
        trial.run_id = Some("evr_foreign".to_string());
        let error = prepare_trial_start("owner-1", &experiment, &trial, &request)
            .expect_err("foreign binding must not be reused");
        assert!(matches!(error, EvaluationBootstrapError::Conflict(_)));
    }

    #[test]
    fn rejects_a_plan_loaded_for_another_owner() {
        let (mut experiment, trial, message) = fixtures();
        let request = EvaluationTrialStartRequest {
            message: Some(message),
            revision_content: None,
            skill_name: None,
            execution_time_budget_secs: None,
            edge_executor_id: None,
        };
        experiment.owner_user_id = "owner-2".to_string();
        let error = prepare_trial_start("owner-1", &experiment, &trial, &request)
            .expect_err("owner-scoped bootstrap must fail closed");
        assert!(matches!(error, EvaluationBootstrapError::Conflict(_)));
    }

    fn prepared_config() -> super::super::execution_config::EvaluationExecutionConfig {
        let mut config =
            crate::evaluation::test_support::execution_config("model-1", "openai", "case-1");
        config.model.model_name = "model-name".into();
        config
    }

    fn prepared_request(kind: EvaluationTargetKind) -> EvaluationExperimentPrepareRequest {
        EvaluationExperimentPrepareRequest {
            submission_idempotency_key: "submit-prepare-1".to_string(),
            target: super::super::api::EvaluationPrepareTarget {
                kind,
                baseline: super::super::api::EvaluationPrepareRevision {
                    revision_id: "base".to_string(),
                    content: Some("baseline prompt".to_string()),
                },
                candidate: super::super::api::EvaluationPrepareRevision {
                    revision_id: "cand".to_string(),
                    content: Some("candidate prompt".to_string()),
                },
                skill_name: None,
            },
            case: super::super::api::EvaluationPrepareCase {
                case_id: "case-1".to_string(),
                message: "fixed input".to_string(),
                holdout: false,
                verifier_config: crate::evaluation::task_verifier::JsonValueEqualsConfig {
                    expected: serde_json::json!({"ok": true}),
                }
                .into(),
            },
            model_offering_id: "model-1".to_string(),
            workspace: None,
            judgment_model_offering_id: None,
            max_concurrency: 2,
            max_wall_time_secs: 30,
        }
    }

    #[test]
    fn prepares_prompt_spec_from_user_content_and_trusted_model_facts() {
        let mut request = prepared_request(EvaluationTargetKind::Prompt);
        request.case.verifier_config = super::super::task_verifier::JsonValueEqualsConfig {
            expected: serde_json::json!({"private_expected_answer": 42}),
        }
        .into();
        let spec = build_prepared_experiment_spec(
            None,
            "evx_prepare",
            &request,
            &prepared_config(),
            None,
            EvaluationJudgmentPolicy::Disabled,
        )
        .expect("prepared spec");
        assert_eq!(spec.plan_trials().expect("trials").len(), 2);
        assert_eq!(spec.conditions.provider_binding, "openai");
        assert_eq!(spec.conditions.execution_config, prepared_config());
        let mut wrong_model = prepared_config();
        wrong_model.model.offering_id = "other-model".into();
        assert!(matches!(
            build_prepared_experiment_spec(
                None,
                "evx_prepare",
                &request,
                &wrong_model,
                None,
                EvaluationJudgmentPolicy::Disabled,
            ),
            Err(EvaluationBootstrapError::Conflict(_))
        ));
        assert_eq!(
            spec.target.baseline.content.as_deref(),
            Some("baseline prompt")
        );
        assert_eq!(spec.cases[0].input_content.as_deref(), Some("fixed input"));
        assert_eq!(
            spec.cases[0].task_verifier,
            super::super::task_verifier::TaskVerifierSpec::freeze(
                request.case.verifier_config.clone()
            )
            .unwrap()
        );
        assert!(prepared_request_matches_spec(&request, &spec));
        let mut changed_verifier = request.clone();
        changed_verifier.case.verifier_config =
            super::super::task_verifier::TaskVerifierConfig::JsonValueEquals {
                expected: serde_json::json!({"private_expected_answer": 43}),
            };
        assert!(!prepared_request_matches_spec(&changed_verifier, &spec));
        assert_eq!(
            spec.target.baseline.content_hash,
            content_fingerprint("baseline prompt")
        );
        let resolved = crate::runs::ResolvedModelSelection {
            offering_id: "model-1".to_string(),
            model_name: "model-name".to_string(),
        };
        let policy = crate::runs::ExecutionPolicyRequest::default();
        assert_eq!(
            spec.conditions.tool_policy_hash,
            evaluation_policy_fingerprint(&EvaluationPolicyFingerprintInput {
                model_binding: "model-1",
                provider_binding: "openai",
                cache_policy: "provider_default_recorded",
                resolved_model_selection: Some(&resolved),
                admitted_provider: "openai",
                admitted_cache_capability: None,
                execution_policy: &policy,
                allow_skills: None,
                allow_skill_sources: None,
                allow_tools: None,
                enabled_tools: None,
                runtime_profile: None,
                workspace_execution: None,
            })
            .unwrap()
        );
    }

    #[test]
    fn prepares_new_skill_comparison_with_an_explicit_no_skill_baseline() {
        let mut request = prepared_request(EvaluationTargetKind::Skill);
        request.target.baseline = super::super::api::EvaluationPrepareRevision {
            revision_id: NO_SKILL_REVISION_ID.to_string(),
            content: None,
        };
        request.target.candidate = super::super::api::EvaluationPrepareRevision {
            revision_id: "candidate-revision".to_string(),
            content: None,
        };
        request.target.skill_name = Some("candidate-skill".to_string());
        let skill = PreparedSkillIdentity {
            skill_name: "candidate-skill".to_string(),
            baseline_revision_id: NO_SKILL_REVISION_ID.to_string(),
            baseline_content_hash: NO_SKILL_CONTENT_HASH.to_string(),
            candidate_revision_id: "candidate-revision".to_string(),
            candidate_content_hash: "sha256:candidate".to_string(),
        };

        let spec = build_prepared_experiment_spec(
            None,
            "evx_new_skill",
            &request,
            &prepared_config(),
            Some(&skill),
            EvaluationJudgmentPolicy::Disabled,
        )
        .expect("new Skill comparison should prepare");

        assert_eq!(
            spec.target.baseline.revision_id,
            NO_SKILL_REVISION_ID.to_string()
        );
        assert_eq!(
            spec.target.baseline.content_hash,
            NO_SKILL_CONTENT_HASH.to_string()
        );
        assert!(spec.target.baseline.content.is_none());
        assert!(prepared_request_matches_spec(&request, &spec));
    }

    fn frozen_workspace_fixture() -> FrozenWorkspaceExecution {
        FrozenWorkspaceExecution {
            edge_executor_id: "edge-a".into(),
            source_commit: "a".repeat(40),
            tool_names: vec!["read_file".into()],
            confinement: serde_json::from_value(json!({
                "profile_id": astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE,
                "toolchain_manifest": {
                    "schema_version": 1,
                    "inputs": [{"guest_mount_path": "/usr/bin", "content_digest": format!("sha256:{}", "a".repeat(64))}],
                    "launcher_digest": format!("sha256:{}", "b".repeat(64)),
                    "supervisor_digest": format!("sha256:{}", "c".repeat(64))
                }
            })).unwrap(),
        }
    }

    #[test]
    fn workspace_freeze_requires_capability_and_retries_preserve_original_contract() {
        let mut request = prepared_request(EvaluationTargetKind::Prompt);
        let workspace = frozen_workspace_fixture();
        request.workspace = Some(super::super::api::EvaluationPrepareWorkspace {
            edge_executor_id: workspace.edge_executor_id.clone(),
            source_commit: workspace.source_commit.to_ascii_uppercase(),
            tool_names: workspace.tool_names.clone(),
        });
        let build = |facts: Option<&FrozenWorkspaceExecution>| {
            build_prepared_experiment_spec(
                facts,
                "evx_confined",
                &request,
                &prepared_config(),
                None,
                EvaluationJudgmentPolicy::Disabled,
            )
        };
        assert!(build(None).is_err());
        let original = build(Some(&workspace)).unwrap();
        let original_bytes = serde_json::to_vec(&original).unwrap();
        let mut changed = workspace.clone();
        changed.confinement.toolchain_manifest.launcher_digest =
            format!("sha256:{}", "d".repeat(64));
        let replacement = build(Some(&changed)).unwrap();
        assert_ne!(
            original.spec_fingerprint().unwrap(),
            replacement.spec_fingerprint().unwrap()
        );
        assert_ne!(
            original.conditions.tool_policy_hash,
            replacement.conditions.tool_policy_hash
        );
        // Retry matching has no live facts input: both offline and changed
        // providers replay exactly the stored snapshot.
        assert!(prepared_request_matches_spec(&request, &original));
        assert_eq!(serde_json::to_vec(&original).unwrap(), original_bytes);
        assert_eq!(
            original.conditions.workspace_execution.as_ref(),
            Some(&workspace)
        );
        changed.edge_executor_id = "edge-other".into();
        assert!(build(Some(&changed)).is_err());
        let mut different_intent = request.clone();
        different_intent.workspace.as_mut().unwrap().source_commit = "e".repeat(40);
        assert!(!prepared_request_matches_spec(&different_intent, &original));
        let mut legacy = serde_json::to_value(&workspace).unwrap();
        legacy.as_object_mut().unwrap().remove("confinement");
        assert!(serde_json::from_value::<FrozenWorkspaceExecution>(legacy).is_err());

        let mut reordered = workspace.clone();
        reordered
            .confinement
            .toolchain_manifest
            .inputs
            .push(astra_runtime_env::ToolchainInput {
                guest_mount_path: "/usr/lib".into(),
                content_digest: format!("sha256:{}", "e".repeat(64)),
            });
        let first = build(Some(&reordered)).unwrap();
        reordered.confinement.toolchain_manifest.inputs.reverse();
        let second = build(Some(&reordered)).unwrap();
        assert_eq!(
            first.spec_fingerprint().unwrap(),
            second.spec_fingerprint().unwrap()
        );
        assert_eq!(
            first.conditions.tool_policy_hash,
            second.conditions.tool_policy_hash
        );
        // Direct Rust construction need not pass through Prepare. Storage
        // deserialization normalizes the manifest, so hashing must do so too.
        let execution_policy = crate::runs::ExecutionPolicyRequest::default();
        let fingerprint = |workspace: &FrozenWorkspaceExecution| {
            evaluation_policy_fingerprint(&EvaluationPolicyFingerprintInput {
                model_binding: "model",
                provider_binding: "provider",
                cache_policy: "default",
                resolved_model_selection: None,
                admitted_provider: "provider",
                admitted_cache_capability: None,
                execution_policy: &execution_policy,
                allow_skills: None,
                allow_skill_sources: None,
                allow_tools: None,
                enabled_tools: None,
                runtime_profile: None,
                workspace_execution: Some(workspace),
            })
            .unwrap()
        };
        let restored: FrozenWorkspaceExecution =
            serde_json::from_slice(&serde_json::to_vec(&reordered).unwrap()).unwrap();
        assert_eq!(fingerprint(&reordered), fingerprint(&restored));
    }

    #[test]
    fn trial_start_uses_the_frozen_edge_workspace_policy() {
        let mut request = prepared_request(EvaluationTargetKind::Prompt);
        request.workspace = Some(super::super::api::EvaluationPrepareWorkspace {
            edge_executor_id: "edge-a".to_string(),
            source_commit: "a".repeat(40),
            tool_names: vec!["read_file".to_string()],
        });
        let workspace = frozen_workspace_fixture();
        let spec = build_prepared_experiment_spec(
            Some(&workspace),
            "evx_edge",
            &request,
            &prepared_config(),
            None,
            EvaluationJudgmentPolicy::Disabled,
        )
        .expect("prepared spec");
        let spec_fingerprint = spec.spec_fingerprint().expect("fingerprint");
        let mut experiment = fixtures().0;
        experiment.spec = spec;
        experiment.spec_fingerprint = spec_fingerprint;
        let trial_unit = experiment
            .spec
            .plan_trials()
            .expect("trials")
            .into_iter()
            .next()
            .expect("first trial");
        let trial = EvaluationTrialBindingRecord {
            owner_user_id: "owner-1".to_string(),
            trial_id: trial_unit.trial_id.clone(),
            experiment_id: experiment.experiment_id.clone(),
            spec_fingerprint: experiment.spec_fingerprint.clone(),
            trial: trial_unit,
            binding_status: "planned".to_string(),
            session_id: None,
            run_id: None,
            run_generation: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        };
        let plan = prepare_trial_start(
            "owner-1",
            &experiment,
            &trial,
            &EvaluationTrialStartRequest {
                message: None,
                revision_content: None,
                skill_name: None,
                execution_time_budget_secs: None,
                edge_executor_id: Some("edge-a".to_string()),
            },
        )
        .expect("trial Edge selection");
        assert_eq!(plan.edge_executor_id.as_deref(), Some("edge-a"));
        assert_eq!(
            plan.workspace_execution,
            experiment.spec.conditions.workspace_execution
        );
        assert_eq!(
            plan.workspace_execution
                .as_ref()
                .map(|workspace| workspace.tool_names.as_slice()),
            Some(["read_file".to_string()].as_slice())
        );
        assert!(
            experiment
                .spec
                .conditions
                .tool_policy_hash
                .starts_with("sha256:")
        );
        assert_eq!(
            experiment.spec.conditions.isolation_profile,
            "edge_workspace_private_v1"
        );
    }

    #[test]
    fn prepares_skill_spec_only_from_owner_scoped_revision_facts() {
        let mut request = prepared_request(EvaluationTargetKind::Skill);
        request.target.skill_name = Some("reviewer".to_string());
        request.target.baseline.content = None;
        request.target.candidate.content = None;
        let spec = build_prepared_experiment_spec(
            None,
            "evx_skill",
            &request,
            &prepared_config(),
            Some(&PreparedSkillIdentity {
                skill_name: "reviewer".to_string(),
                baseline_revision_id: "base".to_string(),
                baseline_content_hash: content_fingerprint("base skill").to_string(),
                candidate_revision_id: "cand".to_string(),
                candidate_content_hash: content_fingerprint("candidate skill").to_string(),
            }),
            EvaluationJudgmentPolicy::Disabled,
        )
        .expect("prepared Skill spec");
        assert_eq!(spec.target.skill_name.as_deref(), Some("reviewer"));
        assert!(spec.target.baseline.content.is_none());
        assert_eq!(
            spec.target.candidate.content_hash,
            content_fingerprint("candidate skill")
        );
    }

    #[test]
    fn starts_prepared_skill_from_frozen_name_when_request_omits_it() {
        let mut request = prepared_request(EvaluationTargetKind::Skill);
        request.target.skill_name = Some("reviewer".to_string());
        request.target.baseline.content = None;
        request.target.candidate.content = None;
        let model = prepared_config();
        let spec = build_prepared_experiment_spec(
            None,
            "evx_skill_start",
            &request,
            &model,
            Some(&PreparedSkillIdentity {
                skill_name: "reviewer".to_string(),
                baseline_revision_id: "base".to_string(),
                baseline_content_hash: content_fingerprint("base skill").to_string(),
                candidate_revision_id: "cand".to_string(),
                candidate_content_hash: content_fingerprint("candidate skill").to_string(),
            }),
            EvaluationJudgmentPolicy::Disabled,
        )
        .expect("prepared Skill spec");
        let spec_fingerprint = spec.spec_fingerprint().expect("fingerprint");
        let trial = spec
            .plan_trials()
            .expect("trials")
            .into_iter()
            .next()
            .expect("trial");
        let experiment = EvaluationExperimentRecord {
            owner_user_id: "owner-1".to_string(),
            experiment_id: spec.experiment_id.clone(),
            spec_fingerprint: spec_fingerprint.clone(),
            spec,
            submission_idempotency_key: request.submission_idempotency_key,
            planned_trial_count: 2,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        };
        let binding = EvaluationTrialBindingRecord {
            owner_user_id: "owner-1".to_string(),
            trial_id: trial.trial_id.clone(),
            experiment_id: experiment.experiment_id.clone(),
            spec_fingerprint,
            trial,
            binding_status: "planned".to_string(),
            session_id: None,
            run_id: None,
            run_generation: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        };
        let plan = prepare_trial_start(
            "owner-1",
            &experiment,
            &binding,
            &EvaluationTrialStartRequest {
                message: None,
                revision_content: None,
                skill_name: None,
                execution_time_budget_secs: None,
                edge_executor_id: None,
            },
        )
        .expect("frozen Skill target should supply the name");
        assert_eq!(
            plan.admission.skill_revision.expect("skill").skill_name,
            "reviewer"
        );
    }

    #[test]
    fn allows_aa_and_rejects_inline_skill_material() {
        let request = prepared_request(EvaluationTargetKind::Prompt);
        let mut same = request.clone();
        same.target.candidate.revision_id = same.target.baseline.revision_id.clone();
        let model = prepared_config();
        let same_spec = build_prepared_experiment_spec(
            None,
            "evx_same",
            &same,
            &model,
            None,
            EvaluationJudgmentPolicy::Disabled,
        )
        .expect("A/A is a valid controlled comparison");
        assert_eq!(
            same_spec.target.baseline.revision_id,
            same_spec.target.candidate.revision_id
        );

        let mut skill = request;
        skill.target.kind = EvaluationTargetKind::Skill;
        skill.target.skill_name = Some("reviewer".to_string());
        assert!(matches!(
            build_prepared_experiment_spec(
                None,
                "evx_inline",
                &skill,
                &model,
                Some(&PreparedSkillIdentity {
                    skill_name: "reviewer".to_string(),
                    baseline_revision_id: "base".to_string(),
                    baseline_content_hash: "sha256:base".to_string(),
                    candidate_revision_id: "cand".to_string(),
                    candidate_content_hash: "sha256:cand".to_string(),
                }),
                EvaluationJudgmentPolicy::Disabled,
            ),
            Err(EvaluationBootstrapError::InvalidInput(_))
        ));
    }

    #[test]
    fn prepared_experiment_id_is_owner_scoped_and_stable() {
        let first = prepared_experiment_id("owner-1", "submission-1").expect("id");
        assert_eq!(
            first,
            prepared_experiment_id("owner-1", "submission-1").unwrap()
        );
        assert_ne!(
            first,
            prepared_experiment_id("owner-2", "submission-1").unwrap()
        );
        assert!(first.starts_with("evx_"));
    }

    #[test]
    fn prepared_retry_matches_frozen_user_intent_without_dynamic_facts() {
        let request = prepared_request(EvaluationTargetKind::Prompt);
        let spec = build_prepared_experiment_spec(
            None,
            "evx_replay",
            &request,
            &prepared_config(),
            None,
            EvaluationJudgmentPolicy::Disabled,
        )
        .expect("spec");
        assert!(prepared_request_matches_spec(&request, &spec));
        let mut changed = request;
        changed.case.message = "different input".to_string();
        assert!(!prepared_request_matches_spec(&changed, &spec));
    }
}
