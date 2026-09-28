//! Service-only Offering fixture for production Auto inheritance tests.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use astra_services::{
    AdmittedModelExecution, ModelAccessKind, ModelCreateRequestData, ModelExecutionPlacement,
    ModelListItem, ModelRecord, ModelService, ModelUpdateRequestData, ResolvedActiveLlmModel,
    ResolvedModelOffering,
    model_routing::{DECISION_KEY, EVENT_TYPE, ModelRoutingDecision},
    runs::{InMemoryRunStateStore, ResolvedModelSelection},
};
use astra_turn_types::{
    ModelSelection,
    model_routing::{AutoModelRoutingPolicy, ModelRoutingReason},
};
use axum::{Json, http::StatusCode};

use crate::server::run::engine::{RunEngine, RunStartContext};

pub(crate) const USER_ID: &str = "uc-user";
pub(crate) const SESSION_ID: &str = "uc-session";
pub(crate) const OFFERING_ID: &str = "genesis-strong";
pub(crate) const MODEL_NAME: &str = "genesis-model";

pub(crate) fn genesis_execution() -> AdmittedModelExecution {
    let mut execution = AdmittedModelExecution::from_offering(ResolvedModelOffering {
        offering_id: OFFERING_ID.into(),
        model: ResolvedActiveLlmModel {
            model_name: MODEL_NAME.into(),
            wire_model_name: Some("genesis-wire-model".into()),
            api_key: "test-genesis-secret".into(),
            base_url: "https://genesis.example/v1".into(),
            provider: "openai".into(),
            fallback_chain: Vec::new(),
            tags: Vec::new(),
            request_body_overrides: None,
            fixed_temperature: None,
            thinking_protocol: None,
            prompt_cache_capability: None,
            thinking_capability: None,
            context_window: Some(128_000),
            max_completion_tokens: Some(16_384),
            request_headers: None,
        },
    })
    .unwrap();
    execution.access_kind = ModelAccessKind::AstraCloud;
    execution.execution_placement = ModelExecutionPlacement::Server;
    execution
}

pub(crate) struct ServiceBackedOffering {
    execution: AdmittedModelExecution,
    calls: Mutex<Vec<(String, String)>>,
    authorized: AtomicBool,
}

impl ServiceBackedOffering {
    pub(crate) fn new(execution: AdmittedModelExecution) -> Self {
        Self {
            execution,
            calls: Mutex::new(Vec::new()),
            authorized: AtomicBool::new(true),
        }
    }

    pub(crate) fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) fn revoke(&self) {
        self.authorized.store(false, Ordering::SeqCst);
    }
}

fn unavailable<T>() -> Result<T, (StatusCode, Json<astra_core::ErrorResponse>)> {
    Err(crate::error_response_coded(
        StatusCode::NOT_FOUND,
        "Genesis Offering unavailable",
        "model_offering_not_found",
    ))
}

#[async_trait::async_trait]
impl ModelService for ServiceBackedOffering {
    async fn admit_model_offering(
        &self,
        user_id: String,
        offering_id: String,
    ) -> Result<AdmittedModelExecution, (StatusCode, Json<astra_core::ErrorResponse>)> {
        self.calls
            .lock()
            .unwrap()
            .push((user_id.clone(), offering_id.clone()));
        if self.authorized.load(Ordering::SeqCst)
            && user_id == USER_ID
            && offering_id == OFFERING_ID
        {
            Ok(self.execution.clone())
        } else {
            unavailable()
        }
    }

    async fn resolve_model_offering(
        &self,
        _: String,
    ) -> Result<ResolvedModelOffering, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unavailable()
    }
    async fn create_model(
        &self,
        _: String,
        _: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unavailable()
    }
    async fn list_models(
        &self,
        _: String,
        _: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unavailable()
    }
    async fn get_model(
        &self,
        _: String,
    ) -> Result<ModelRecord, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unavailable()
    }
    async fn update_model(
        &self,
        _: String,
        _: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unavailable()
    }
    async fn delete_model(
        &self,
        _: String,
    ) -> Result<(), (StatusCode, Json<astra_core::ErrorResponse>)> {
        unavailable()
    }
    async fn check_model(
        &self,
        _: String,
    ) -> Result<ModelRecord, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unavailable()
    }
}

pub(crate) async fn auto_parent_run(run_id: &str, execution: &AdmittedModelExecution) -> RunEngine {
    let engine = RunEngine::new(std::sync::Arc::new(InMemoryRunStateStore::new()));
    let authority = engine
        .start_run_ext_with_context(
            run_id,
            USER_ID,
            SESSION_ID,
            None,
            None,
            None,
            None,
            RunStartContext {
                model_selection: Some(ModelSelection {
                    offering_id: OFFERING_ID.into(),
                }),
                resolved_model_selection: Some(ResolvedModelSelection {
                    offering_id: OFFERING_ID.into(),
                    model_name: MODEL_NAME.into(),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let decision = ModelRoutingDecision {
        schema_version: 1,
        work_admission: None,
        work_admission_skill_revision: 0,
        policy_version: astra_turn_core::model_routing::POLICY_VERSION.into(),
        policy: AutoModelRoutingPolicy {
            revision: "test-revision".into(),
            economy_offering_id: "genesis-economy".into(),
            strong_offering_id: OFFERING_ID.into(),
        },
        run_id: run_id.into(),
        session_id: SESSION_ID.into(),
        selected_offering_id: OFFERING_ID.into(),
        selected_model: MODEL_NAME.into(),
        selected_contract_root: super::model_execution_contract_root(execution),
        input_reference: None,
        reason: ModelRoutingReason::StrongRequired,
        assessment: None,
    };
    assert!(
        engine
            .append_events_if_current_generation_and_status(
                USER_ID,
                SESSION_ID,
                run_id,
                authority.owner_generation,
                &["running"],
                &[serde_json::json!({
                    "event_type": EVENT_TYPE,
                    "idempotency_key": DECISION_KEY,
                    "data": decision,
                })],
            )
            .await
            .unwrap()
    );
    engine
}
