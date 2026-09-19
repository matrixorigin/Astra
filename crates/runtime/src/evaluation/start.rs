//! Runtime adapter for starting one frozen evaluation trial.
//!
//! The preparation and identities live in `astra-services::evaluation`; this
//! module only connects that plan to the existing SessionService and Run
//! lifecycle. It must not persist a second execution state machine.

use std::collections::HashMap;

use axum::{Json, http::StatusCode};
use serde_json::{Map, Value};

use crate::AppState;
use astra_core::{ErrorResponse, error_response, error_response_coded};
use astra_services::evaluation::{
    DatabaseEvaluationPlanStore, EvaluationBootstrapError, EvaluationTrialStartRequest,
    EvaluationTrialStartResponse, prepare_trial_start,
};
use astra_services::runs::{
    ChatRequestData, ExecutionPolicyRequest, ExecutionTimeBudget, ModelSelectionMode,
    RunStartIdempotency, RunStartIdempotencyKind,
};
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

fn build_chat_request(
    plan: astra_services::evaluation::EvaluationTrialStartPlan,
    session_id: String,
) -> Result<ChatRequestData, (StatusCode, Json<ErrorResponse>)> {
    let run_start_idempotency = RunStartIdempotency::new(
        RunStartIdempotencyKind::EvaluationTrial,
        plan.run_id,
        plan.request_fingerprint,
    )
    .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let model_offering_id = plan.model_offering_id;
    Ok(ChatRequestData {
        message: plan.message,
        user_intent: None,
        parts: Vec::new(),
        attachments: Vec::new(),
        stable_runtime_system_prompt: plan.revision_content,
        runtime_system_prompt: None,
        session_id: Some(session_id),
        work_binding: None,
        run_start_idempotency: Some(run_start_idempotency),
        evaluation_admission: Some(plan.admission),
        full_llm_capture: false,
        agent_id: None,
        model: None,
        model_selection_mode: ModelSelectionMode::ExplicitOffering,
        model_selection: Some(ModelSelection {
            offering_id: model_offering_id,
        }),
        resolved_model_selection: None,
        admitted_model_execution: None,
        capability_descriptors: None,
        provider_runtime_authorized: false,
        agent_bindings: Vec::new(),
        agent_binding: None,
        runtime_auth: None,
        runtime_skill_binding: None,
        runtime_profile: None,
        skill_search: None,
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        enabled_tools: None,
        workspace_binding: None,
        executor_binding: None,
        execution_binding_generation: None,
        runtime_mcp_bindings: Vec::new(),
        context: None,
        edge_executor_id: plan.edge_executor_id,
        capabilities: Vec::new(),
        forward_headers: HashMap::new(),
        provider_run_owner: None,
        provider_workspace_id: None,
        execution_budget: None,
        execution_time_budget: Some(ExecutionTimeBudget {
            remaining_seconds: plan.execution_time_budget_secs,
        }),
        admitted_execution_deadline: None,
        execution_policy: ExecutionPolicyRequest::default(),
        explain: false,
        interaction_mode: None,
        interactive_client: false,
        conversation_authority: None,
    })
}

pub async fn start_trial(
    state: &AppState,
    owner_user_id: &str,
    experiment_id: &str,
    trial_id: &str,
    request: EvaluationTrialStartRequest,
) -> Result<EvaluationTrialStartResponse, (StatusCode, Json<ErrorResponse>)> {
    let pool = state.shared_pool.clone().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "evaluation database is not configured",
        )
    })?;
    let plan_store = DatabaseEvaluationPlanStore::new(pool);
    let experiment = plan_store
        .load_experiment(owner_user_id, experiment_id)
        .await
        .map_err(map_persistence_error)?;
    let trial = plan_store
        .load_trial(owner_user_id, trial_id)
        .await
        .map_err(map_persistence_error)?;
    let plan = prepare_trial_start(owner_user_id, &experiment, &trial, &request)
        .map_err(map_bootstrap_error)?;

    let mut metadata = Map::new();
    metadata.insert(
        "evaluation_experiment_id".to_string(),
        Value::String(plan.experiment_id.clone()),
    );
    metadata.insert(
        "evaluation_trial_id".to_string(),
        Value::String(plan.trial_id.clone()),
    );
    metadata.insert(
        "evaluation_start_fingerprint".to_string(),
        Value::String(plan.request_fingerprint.clone()),
    );
    let session =
        crate::server::session::session_quota::create_idempotent_session_with_resource_quota(
            state,
            owner_user_id.to_string(),
            plan.session_id.clone(),
            astra_services::SessionCreateRequestData {
                agent_id: None,
                title: Some(format!("Eval {}", plan.trial_id)),
                metadata: Some(metadata),
            },
            plan.request_fingerprint.clone(),
        )
        .await?;
    let chat_request = build_chat_request(plan, session.session_id.clone())?;
    let run = state
        .execution
        .run_lifecycle_service
        .create_run(owner_user_id.to_string(), chat_request)
        .await?;
    Ok(EvaluationTrialStartResponse {
        experiment_id: experiment_id.to_string(),
        trial_id: trial_id.to_string(),
        session_id: run.session_id,
        run_id: run.run_id,
        status: run.status,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::evaluation::{EvaluationRunAdmission, EvaluationTrialStartPlan};

    #[test]
    fn chat_request_keeps_trusted_evaluation_fields_server_owned() {
        let hash = "a".repeat(64);
        let plan = EvaluationTrialStartPlan {
            experiment_id: "exp-1".to_string(),
            trial_id: "trial-1".to_string(),
            session_id: "evs_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            run_id: "evr_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            request_fingerprint: hash.clone(),
            message: "fixed input".to_string(),
            revision_content: Some("fixed prompt".to_string()),
            model_offering_id: "offering-1".to_string(),
            execution_time_budget_secs: 30,
            edge_executor_id: None,
            admission: EvaluationRunAdmission {
                experiment_id: "exp-1".to_string(),
                trial_id: "trial-1".to_string(),
                input_content_hash: hash.clone(),
                revision_content_hash: hash,
                skill_revision: None,
                receipt_ids: Vec::new(),
                snapshot_envelope: None,
            },
        };
        let request = build_chat_request(plan, "evs_session".to_string()).expect("request");
        assert_eq!(request.session_id.as_deref(), Some("evs_session"));
        assert_eq!(request.message, "fixed input");
        assert!(request.parts.is_empty());
        assert!(request.attachments.is_empty());
        assert!(request.context.is_none());
        assert!(request.allow_tools.is_none());
        assert!(request.evaluation_admission.is_some());
        assert_eq!(
            request
                .run_start_idempotency
                .as_ref()
                .map(|identity| identity.kind()),
            Some(RunStartIdempotencyKind::EvaluationTrial)
        );
    }

    #[test]
    fn chat_request_carries_only_the_edge_selection_intent() {
        let hash = "b".repeat(64);
        let plan = EvaluationTrialStartPlan {
            experiment_id: "exp-edge".to_string(),
            trial_id: "trial-edge".to_string(),
            session_id: "evs_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                .to_string(),
            run_id: "evr_dddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
            request_fingerprint: hash.clone(),
            message: "fixed input".to_string(),
            revision_content: Some("fixed prompt".to_string()),
            model_offering_id: "offering-1".to_string(),
            execution_time_budget_secs: 30,
            edge_executor_id: Some("edge-a".to_string()),
            admission: EvaluationRunAdmission {
                experiment_id: "exp-edge".to_string(),
                trial_id: "trial-edge".to_string(),
                input_content_hash: hash.clone(),
                revision_content_hash: hash,
                skill_revision: None,
                receipt_ids: Vec::new(),
                snapshot_envelope: None,
            },
        };
        let request = build_chat_request(plan, "evs_edge".to_string()).expect("Edge request");
        assert!(request.workspace_binding.is_none());
        assert!(request.executor_binding.is_none());
        assert_eq!(request.edge_executor_id.as_deref(), Some("edge-a"));
    }
}
