//! User-facing evaluation preparation.
//!
//! This is the only layer that turns an authenticated request into a frozen
//! evaluation plan. It resolves model and owner-scoped Skill facts first,
//! then registers the immutable spec through the existing plan store. It does
//! not create a session, allocate a provider slot, or introduce a scheduler.

use axum::{Json, http::StatusCode};

use super::workspace::freeze_workspace_capability;
use crate::AppState;
use astra_core::{ErrorResponse, error_response, error_response_coded};
use astra_services::evaluation::{
    DatabaseEvaluationPlanStore, EVALUATION_ADAPTER_PROFILE_VERSION, EvaluationBootstrapError,
    EvaluationExperimentPrepareRequest, EvaluationExperimentPrepareResponse,
    EvaluationJudgmentPolicy, EvaluationTargetKind, FrozenSkillRoutingPolicy,
    NO_SKILL_CONTENT_HASH, NO_SKILL_REVISION_ID, PreparedSkillIdentity,
    build_prepared_experiment_spec, prepared_experiment_id, prepared_request_matches_spec,
};
use astra_services::{DatabasePersonalSkillStore, PersonalSkillError};
use astra_turn_types::ModelSelection;

fn map_bootstrap_error(error: EvaluationBootstrapError) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        EvaluationBootstrapError::InvalidInput(detail) => {
            error_response(StatusCode::BAD_REQUEST, detail)
        }
        EvaluationBootstrapError::Conflict(detail) => error_response(StatusCode::CONFLICT, detail),
        EvaluationBootstrapError::Unsupported(detail) => {
            error_response(StatusCode::NOT_IMPLEMENTED, detail)
        }
    }
}

fn map_persistence_error(
    error: astra_services::evaluation::EvaluationPersistenceError,
) -> (StatusCode, Json<ErrorResponse>) {
    use astra_services::evaluation::EvaluationPersistenceError;
    match error {
        EvaluationPersistenceError::InvalidInput(detail) => {
            error_response(StatusCode::BAD_REQUEST, detail)
        }
        EvaluationPersistenceError::Conflict(detail) => {
            error_response(StatusCode::CONFLICT, detail)
        }
        EvaluationPersistenceError::NotFound(detail) => {
            error_response(StatusCode::NOT_FOUND, detail)
        }
        other => error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            other.to_string(),
            "evaluation_store_unavailable",
        ),
    }
}

fn map_skill_error(error: PersonalSkillError) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        PersonalSkillError::InvalidStatus { .. } => {
            error_response(StatusCode::BAD_REQUEST, error.to_string())
        }
        PersonalSkillError::VersionNotFound { .. } => {
            error_response(StatusCode::NOT_FOUND, error.to_string())
        }
        PersonalSkillError::VersionNotActivatable { .. } => {
            error_response(StatusCode::CONFLICT, error.to_string())
        }
        other => error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            other.to_string(),
            "skill_store_unavailable",
        ),
    }
}

async fn resolve_workspace_capability(
    state: &AppState,
    owner_user_id: &str,
    request: &EvaluationExperimentPrepareRequest,
) -> Result<
    Option<astra_services::evaluation::FrozenWorkspaceExecution>,
    (StatusCode, Json<ErrorResponse>),
> {
    let Some(intent) = request.workspace.as_ref() else {
        return Ok(None);
    };
    let unavailable = |detail: &str| {
        error_response_coded(
            StatusCode::PRECONDITION_FAILED,
            detail,
            "evaluation_workspace_confinement_unavailable",
        )
    };
    let pool = &state.edge_connection_pool;
    let connected = pool
        .find_user_edge_by_agent_and_workspace(owner_user_id, &intent.edge_executor_id, None)
        .ok_or_else(|| unavailable("selected Edge is not connected for this owner"))?;
    let record = state
        .execution
        .edge_registry_service
        .find_by_user_agent_and_workspace(owner_user_id, &intent.edge_executor_id, None)
        .await
        .map_err(|_| unavailable("selected Edge registry is unavailable"))?
        .ok_or_else(|| unavailable("selected Edge is not registered for this owner"))?;
    let current = pool
        .find_user_edge_by_agent_and_workspace(owner_user_id, &intent.edge_executor_id, None)
        .filter(|current| current.generation == connected.generation)
        .ok_or_else(|| unavailable("selected Edge connection changed during preparation"))?;
    freeze_workspace_capability(owner_user_id, intent, &current, &record)
        .map(Some)
        .map_err(|detail| unavailable(&detail))
}

pub async fn prepare_experiment(
    state: &AppState,
    owner_user_id: &str,
    request: EvaluationExperimentPrepareRequest,
) -> Result<EvaluationExperimentPrepareResponse, (StatusCode, Json<ErrorResponse>)> {
    let pool = state.shared_pool.clone().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "evaluation database is not configured",
        )
    })?;
    let experiment_id = prepared_experiment_id(owner_user_id, &request.submission_idempotency_key)
        .map_err(map_bootstrap_error)?;
    let plan_store = DatabaseEvaluationPlanStore::new(pool.clone());
    match plan_store
        .load_experiment(owner_user_id, &experiment_id)
        .await
    {
        Ok(experiment) => {
            if experiment.submission_idempotency_key != request.submission_idempotency_key
                || !prepared_request_matches_spec(&request, &experiment.spec)
            {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "submission key already belongs to a different frozen evaluation intent",
                ));
            }
            let trials = plan_store
                .list_trials(owner_user_id, &experiment.experiment_id)
                .await
                .map_err(map_persistence_error)?;
            return Ok(EvaluationExperimentPrepareResponse {
                experiment,
                trials,
                adapter_profile_version: EVALUATION_ADAPTER_PROFILE_VERSION.to_string(),
            });
        }
        Err(astra_services::evaluation::EvaluationPersistenceError::NotFound(_)) => {}
        Err(error) => return Err(map_persistence_error(error)),
    }

    // Exact durable retries above return their original snapshot without
    // consulting today's live capability or replacing the frozen toolchain.
    let workspace = resolve_workspace_capability(state, owner_user_id, &request).await?;

    let admitted = crate::server::model_execution_admission::admit_model_execution(
        &state.model_service,
        astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
        owner_user_id,
        &ModelSelection {
            offering_id: request.model_offering_id.clone(),
        },
        None,
        None,
        None,
    )
    .await?;

    let skill = if matches!(
        request.target.kind,
        EvaluationTargetKind::Skill | EvaluationTargetKind::SkillRoutingJudgment
    ) {
        let skill_name = request.target.skill_name.as_deref().ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "Skill preparation requires skill_name",
            )
        })?;
        let store = DatabasePersonalSkillStore::new(pool.clone());
        let baseline = if request.target.baseline.revision_id == NO_SKILL_REVISION_ID {
            None
        } else {
            Some(
                store
                    .load_version(
                        owner_user_id,
                        skill_name,
                        &request.target.baseline.revision_id,
                    )
                    .await
                    .map_err(map_skill_error)?
                    .ok_or_else(|| {
                        error_response(
                            StatusCode::NOT_FOUND,
                            "baseline Skill revision was not found for this user",
                        )
                    })?,
            )
        };
        let candidate = store
            .load_version(
                owner_user_id,
                skill_name,
                &request.target.candidate.revision_id,
            )
            .await
            .map_err(map_skill_error)?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    "candidate Skill revision was not found for this user",
                )
            })?;
        if candidate.version_id == NO_SKILL_REVISION_ID
            || !matches!(candidate.status.as_str(), "published" | "draft")
            || baseline
                .as_ref()
                .is_some_and(|revision| revision.status != "published")
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "evaluation requires a published baseline and a published or evaluation-draft candidate",
            ));
        }
        if candidate.owner_user_id != owner_user_id
            || candidate.skill_name != skill_name
            || baseline.as_ref().is_some_and(|revision| {
                revision.owner_user_id != owner_user_id || revision.skill_name != skill_name
            })
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "Skill revision ownership does not match the authenticated user",
            ));
        }
        for revision in baseline.iter().chain(std::iter::once(&candidate)) {
            if revision.content_hash
                != astra_services::skill_md_content_hash(
                    &revision.manifest_json,
                    &revision.content_markdown,
                )
            {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "Skill revision content hash does not match its stored manifest and content",
                ));
            }
            crate::turn::skill_tool::PinnedSkillResolver::from_user_skill_revision(revision)
                .map_err(|detail| {
                    error_response(
                        StatusCode::NOT_IMPLEMENTED,
                        format!("Skill revision is outside the evaluation adapter: {detail}"),
                    )
                })?;
        }
        Some(PreparedSkillIdentity {
            skill_name: skill_name.to_string(),
            baseline_revision_id: baseline
                .as_ref()
                .map(|revision| revision.version_id.clone())
                .unwrap_or_else(|| NO_SKILL_REVISION_ID.to_string()),
            baseline_content_hash: baseline
                .as_ref()
                .map(|revision| revision.content_hash.clone())
                .unwrap_or_else(|| NO_SKILL_CONTENT_HASH.to_string()),
            candidate_revision_id: candidate.version_id,
            candidate_content_hash: candidate.content_hash,
        })
    } else {
        None
    };

    let judgment_policy = if request.target.kind == EvaluationTargetKind::SkillRoutingJudgment {
        let admitted_judgment =
            if let Some(offering_id) = request.judgment_model_offering_id.as_deref() {
                crate::server::model_execution_admission::admit_model_execution(
                    &state.model_service,
                    astra_core::model_wire::purpose::ModelRequestPurpose::TypedJudgment,
                    owner_user_id,
                    &ModelSelection {
                        offering_id: offering_id.to_string(),
                    },
                    None,
                    None,
                    None,
                )
                .await
                .map(Some)?
            } else {
                astra_services::admin_config::resolve_judgment_offering(
                    state.admin.config_service.as_ref(),
                    state.model_service.as_ref(),
                    owner_user_id,
                )
                .await?
            };
        let frozen = match admitted_judgment {
            Some(admitted) => {
                astra_services::models::validate_model_execution_purpose(
                    &admitted,
                    astra_core::model_wire::purpose::ModelRequestPurpose::TypedJudgment,
                )
                .map_err(|(_, body)| error_response(StatusCode::BAD_REQUEST, body.0.detail))?;
                crate::turn::execution_config::freeze_skill_routing_policy(
                    &admitted,
                    &state.fernet_encryptor,
                )
                .map_err(|detail| {
                    error_response_coded(
                        StatusCode::CONFLICT,
                        detail,
                        "evaluation_judgment_policy_unavailable",
                    )
                })?
            }
            None => FrozenSkillRoutingPolicy::unavailable("judgment_offering_unconfigured"),
        };
        EvaluationJudgmentPolicy::SkillRouting {
            candidate: Box::new(frozen),
        }
    } else {
        EvaluationJudgmentPolicy::Disabled
    };

    let execution_config =
        crate::turn::execution_config::PreparedExecutionInputs::freeze_for_prepare(
            &admitted,
            &state.fernet_encryptor,
            &request.case.case_id,
            &request.case.message,
        )
        .map_err(|detail| {
            error_response_coded(
                StatusCode::CONFLICT,
                detail,
                "evaluation_execution_config_unavailable",
            )
        })?;
    let spec = build_prepared_experiment_spec(
        workspace.as_ref(),
        &experiment_id,
        &request,
        &execution_config,
        skill.as_ref(),
        judgment_policy,
    )
    .map_err(map_bootstrap_error)?;
    let experiment = match plan_store
        .register_experiment(owner_user_id, &spec, &request.submission_idempotency_key)
        .await
    {
        Ok(experiment) => experiment,
        Err(error @ astra_services::evaluation::EvaluationPersistenceError::Conflict(_)) => {
            // A concurrent first submission may have won between the initial
            // read and registration. Re-read that immutable winner and replay
            // it when the user intent is the same; do not turn a race into a
            // false conflict merely because model admission completed in a
            // different order.
            match plan_store
                .load_experiment(owner_user_id, &experiment_id)
                .await
            {
                Ok(existing)
                    if existing.submission_idempotency_key
                        == request.submission_idempotency_key
                        && prepared_request_matches_spec(&request, &existing.spec) =>
                {
                    existing
                }
                Ok(_)
                | Err(astra_services::evaluation::EvaluationPersistenceError::NotFound(_)) => {
                    return Err(map_persistence_error(error));
                }
                Err(read_error) => return Err(map_persistence_error(read_error)),
            }
        }
        Err(error) => return Err(map_persistence_error(error)),
    };
    let trials = plan_store
        .list_trials(owner_user_id, &experiment.experiment_id)
        .await
        .map_err(map_persistence_error)?;
    Ok(EvaluationExperimentPrepareResponse {
        experiment,
        trials,
        adapter_profile_version: EVALUATION_ADAPTER_PROFILE_VERSION.to_string(),
    })
}
