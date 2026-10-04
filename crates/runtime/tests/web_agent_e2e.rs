#![cfg(feature = "e2e-hooks")]
//! Web agent mode E2E tests — incremental SSE streaming, edge tool delivery via ledger.
//!
//! These tests exercise the `/chat/stream` → `ServerAgenticLoopHost` path (NOT the bridge),
//! using native loopback provider responses through the real Server execution path.
//!
//! ```text
//! cargo test -p astra-runtime --test web_agent_e2e --features e2e-hooks
//! ```

mod test_support;

use astra_runtime::server::provider_test_support::{
    InferenceLedgerFixture, ProviderGateway, ProviderRequest, ProviderResponse, ProviderScript,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthPrincipal, AuthPrincipalOrigin, AuthRefreshRequestData,
    AuthRegisterRequestData, AuthService, AuthTokenRecord, AuthUserRecord, ErrorResponse,
    HealthChecker, ServiceInfo, SessionActivityRecord, SessionCreateRequestData, SessionListFilter,
    SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
    TurnAuxiliaryEventRecord, TurnAuxiliaryEventWriter, TurnHookDbPersistPlan, TurnHookDbWriter,
    TurnObserverRequest, TurnObserverWorker, build_app,
};
use astra_services::skills::{
    SkillInfoRecord, SkillListCursor, SkillListItem, SkillListRecord, SkillPublishRequestData,
    SkillRecord, SkillService, SkillStatusRecord, SkillVersionRecord,
};
use astra_services::{
    AgentBindingCreateRequestData, AgentBindingOwnerScope, AgentBindingPayload,
    AgentBindingService, AuthProviderAuthorizedRequestContext, InMemoryAgentBindingService,
    ModelCreateRequestData, ModelListItem, ModelRecord, ModelService, ModelUpdateRequestData,
    ProviderRequestDescriptor, ResolvedActiveLlmModel, ResolvedModelOffering,
};
use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
    routing::post,
};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tower::util::ServiceExt;

use crate::test_support::{
    parse_sse_events, test_fernet_encryptor, test_run_lifecycle, tool_call, tool_schema,
};

// ── Env setup ────────────────────────────────────────────────────────────────

const SECRET: &str = "web-agent-e2e-secret";
const TOKEN: &str = "Bearer web-agent-e2e-token";
const PROVIDER_TOKEN: &str = "Bearer web-agent-e2e-provider-token";
const PROVIDER_ID: &str = "test-provider";
const USER_ID: &str = "web-agent-e2e-user";
const DEFAULT_MODEL_OFFERING_ID: &str = "model-test-model";
const DEFAULT_TEST_EDGE_AGENT_ID: &str = "web-agent-e2e-edge";
const DEFAULT_TOOL_RESULT_SESSION_ID: &str = "web-agent-e2e-session";
const DEFAULT_TOOL_RESULT_RUN_ID: &str = "web-agent-e2e-run";
const DEFAULT_TOOL_RESULT_TURN_CHAIN_ID: &str = "web-agent-e2e-turn-chain";

static SECRET_INIT: OnceLock<()> = OnceLock::new();
#[derive(Clone, Debug)]
struct ToolResultIdentity {
    session_id: String,
    run_id: String,
    turn_chain_id: String,
}

fn tool_request_identity_from_event(event: &Value) -> Option<(String, ToolResultIdentity)> {
    let request_id = event.get("request_id")?.as_str()?.to_string();
    let session_id = event.get("session_id")?.as_str()?.to_string();
    let run_id = event.get("run_id")?.as_str()?.to_string();
    let turn_chain_id = event.get("turn_chain_id")?.as_str()?.to_string();
    if request_id.is_empty()
        || session_id.is_empty()
        || run_id.is_empty()
        || turn_chain_id.is_empty()
    {
        return None;
    }
    Some((
        request_id,
        ToolResultIdentity {
            session_id,
            run_id,
            turn_chain_id,
        },
    ))
}

fn unmatched_tool_result_identity() -> ToolResultIdentity {
    ToolResultIdentity {
        session_id: DEFAULT_TOOL_RESULT_SESSION_ID.to_string(),
        run_id: DEFAULT_TOOL_RESULT_RUN_ID.to_string(),
        turn_chain_id: DEFAULT_TOOL_RESULT_TURN_CHAIN_ID.to_string(),
    }
}

fn init_env() {
    SECRET_INIT.get_or_init(|| unsafe {
        std::env::set_var("ASTRA_TEST_E2E_SECRET", SECRET);
    });
}

// ── Stubs ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StubHealth;
#[async_trait]
impl HealthChecker for StubHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct StubAuth;
#[async_trait]
impl AuthService for StubAuth {
    async fn current_user(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != TOKEN {
            return Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("unauthorized")),
            ));
        }
        Ok(AuthUserRecord {
            user_id: USER_ID.into(),
            username: "web-e2e".into(),
            email: "web-e2e@test.com".into(),
            display_name: None,
        })
    }

    async fn current_principal_for_request(
        &self,
        headers: &HeaderMap,
        _request: ProviderRequestDescriptor,
    ) -> Result<AuthPrincipal, (StatusCode, axum::Json<ErrorResponse>)> {
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        if authorization == Some(PROVIDER_TOKEN) {
            return Ok(AuthPrincipal {
                user: AuthUserRecord {
                    user_id: format!("provider_authorized:{PROVIDER_ID}:web-agent-e2e-user"),
                    username: "web-agent-e2e-user".to_string(),
                    email: String::new(),
                    display_name: None,
                },
                session_id: None,
                origin: AuthPrincipalOrigin::ProviderAuthorizedRequest(
                    AuthProviderAuthorizedRequestContext {
                        provider_id: PROVIDER_ID.to_string(),
                        external_subject: "web-agent-e2e-user".to_string(),
                        provider_scope_id: "web-agent-e2e-workspace".to_string(),
                        request_authorization_id: "web-agent-e2e-authorization".to_string(),
                        edge_agent_id: None,
                    },
                ),
            });
        }
        self.current_principal(headers).await
    }
    async fn register(
        &self,
        _: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
}

#[derive(Clone)]
struct StubSession;
#[async_trait]
impl SessionService for StubSession {
    async fn create_session(
        &self,
        _: String,
        _: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id: format!("web-e2e-{}", uuid::Uuid::new_v4()),
            user_id: String::new(),
            agent_id: None,
            title: None,
            status: "active".into(),
            metadata: serde_json::Map::from_iter([(
                "full_llm_capture".into(),
                serde_json::Value::Bool(false),
            )]),
            event_count: 0,
            created_at: String::new(),
            updated_at: None,
            ended_at: None,
        })
    }
    async fn get_session(
        &self,
        session_id: String,
        _: String,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id,
            user_id: String::new(),
            agent_id: None,
            title: None,
            status: "active".into(),
            metadata: serde_json::Map::from_iter([(
                "full_llm_capture".into(),
                serde_json::Value::Bool(false),
            )]),
            event_count: 0,
            created_at: String::new(),
            updated_at: None,
            ended_at: None,
        })
    }
    async fn update_session(
        &self,
        _: String,
        _: String,
        _: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn list_sessions(
        &self,
        _: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn delete_session(
        &self,
        _: String,
        _: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn get_session_activity(
        &self,
        _session_id: String,
        _user_id: String,
        _limit: u32,
        _cursor: Option<astra_services::auth::SessionActivityCursor>,
    ) -> Result<SessionActivityRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionActivityRecord {
            session_id: "stub".into(),
            activities: vec![],
            total: 0,
            limit: _limit,
            next_cursor: None,
        })
    }
}

// ── Recording test doubles ───────────────────────────────────────────────────

/// Records all hook DB persist calls for test verification.
#[derive(Default)]
struct RecordingHookDbWriter {
    plans: tokio::sync::Mutex<Vec<TurnHookDbPersistPlan>>,
}

#[async_trait]
impl TurnHookDbWriter for RecordingHookDbWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        self.plans.lock().await.push(plan);
        Ok(())
    }
}

/// Records all observer requests for test verification.
#[derive(Default)]
struct RecordingObserverWorker {
    requests: tokio::sync::Mutex<Vec<TurnObserverRequest>>,
}

#[async_trait]
impl TurnObserverWorker for RecordingObserverWorker {
    async fn run(&self, request: TurnObserverRequest) -> Result<(), String> {
        self.requests.lock().await.push(request);
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct NoopAuxiliaryEventWriter;

#[async_trait]
impl TurnAuxiliaryEventWriter for NoopAuxiliaryEventWriter {
    async fn persist_events(&self, _events: Vec<TurnAuxiliaryEventRecord>) -> Result<(), String> {
        Ok(())
    }
}

struct TestSkillService;

#[async_trait]
impl SkillService for TestSkillService {
    async fn list_skills(
        &self,
        _user_id: String,
        limit: u32,
        cursor: Option<SkillListCursor>,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
        if cursor.is_some() {
            return Ok(SkillListRecord {
                skills: Vec::new(),
                total: Some(1),
                limit,
                next_cursor: None,
            });
        }

        Ok(SkillListRecord {
            skills: vec![SkillListItem {
                skill_id: "test-skill@1.0.0".to_string(),
                skill_name: "test-skill".to_string(),
                version: "1.0.0".to_string(),
                description: Some("Test skill".to_string()),
                status: Some("active".to_string()),
                source: Some("user".to_string()),
                category: Some("testing".to_string()),
                created_at: None,
            }],
            total: Some(1),
            limit,
            next_cursor: None,
        })
    }

    async fn get_skill(
        &self,
        _user_id: String,
        skill_id: String,
        _version: Option<String>,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
        if skill_id == "test-skill" || skill_id == "test-skill@1.0.0" {
            return Ok(SkillRecord {
                skill_id: "test-skill@1.0.0".to_string(),
                skill_name: "test-skill".to_string(),
                version: "1.0.0".to_string(),
                description: Some("Test skill".to_string()),
                metadata: Some(json!({
                    "skill_type": "local",
                    "instructions": "You are the test skill. Return the prepared instructions.",
                    "when_to_use": "when validating skill interception"
                })),
                created_at: None,
            });
        }

        Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not found".to_string())),
        ))
    }

    async fn get_skill_info(
        &self,
        _: String,
        _: String,
    ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn list_skill_versions(
        &self,
        _: String,
        _: String,
    ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn get_skill_status(
        &self,
        _: String,
        _: u32,
    ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn publish_skill(
        &self,
        _: String,
        _: SkillPublishRequestData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn unpublish_skill(
        &self,
        _: String,
        _: String,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
}

#[derive(Default)]
struct TestModelService {
    judgment_base_url: Option<String>,
}

impl TestModelService {
    fn model_record(&self, name: String) -> ModelRecord {
        let mut model = test_model_record(name);
        if let Some(url) = &self.judgment_base_url {
            model.provider = "openai".to_string();
            model.base_url = Some(url.clone());
        }
        model
    }
}

fn test_model_record(name: String) -> ModelRecord {
    ModelRecord {
        model_id: format!("model-{name}"),
        name,
        provider: "mock".to_string(),
        base_url: Some("http://127.0.0.1:1".to_string()),
        description: None,
        is_active: true,
        context_window: 128_000,
        max_completion_tokens: None,
        input_modalities: Vec::new(),
        output_modalities: Vec::new(),
        supported_parameters: Vec::new(),
        pricing: Default::default(),
        architecture: None,
        tags: Vec::new(),
        quirks: Default::default(),
        connectivity: None,
        thinking_capability: None,
        thinking_probe: None,
    }
}

#[async_trait]
impl ModelService for TestModelService {
    async fn create_model(
        &self,
        _: String,
        _: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn list_models(
        &self,
        _: String,
        _: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        let model = self.model_record("test-model".to_string());
        Ok(vec![ModelListItem {
            thinking_protocol: None,
            offering_id: DEFAULT_MODEL_OFFERING_ID.to_string(),
            access_id: "web-e2e-model-access".to_string(),
            access_kind: astra_services::models::ModelAccessKind::CloudByok,
            access_label: "test".to_string(),
            execution_placement: astra_services::models::ModelExecutionPlacement::Server,
            name: model.name,
            provider: model.provider,
            description: model.description,
            is_active: model.is_active,
            context_window: model.context_window,
            max_completion_tokens: model.max_completion_tokens,
            architecture: model.architecture,
            thinking_capability: model.thinking_capability,
            pricing: None,
        }])
    }

    async fn get_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(self.model_record(model_name))
    }

    async fn resolve_model_offering(
        &self,
        offering_id: String,
    ) -> Result<ResolvedModelOffering, (StatusCode, Json<ErrorResponse>)> {
        let model_name = offering_id
            .strip_prefix("model-")
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("offering not found")),
                )
            })?
            .to_string();
        Ok(ResolvedModelOffering {
            offering_id,
            model: ResolvedActiveLlmModel {
                model_name,
                wire_model_name: None,
                api_key: "test-provider-secret".to_string(),
                base_url: self
                    .judgment_base_url
                    .clone()
                    .unwrap_or_else(|| "http://127.0.0.1:1".to_string()),
                provider: if self.judgment_base_url.is_some() {
                    "openai"
                } else {
                    "mock"
                }
                .to_string(),
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
                price_snapshot: None,
            },
        })
    }

    async fn update_model(
        &self,
        _: String,
        _: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn delete_model(&self, _: String) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn check_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(self.model_record(model_name))
    }
}

// ── App builder ──────────────────────────────────────────────────────────────

fn build_test_app() -> (Router, Arc<tokio::sync::Mutex<HashMap<String, Value>>>) {
    build_test_app_with_models(Arc::new(TestModelService::default()), None)
}

fn build_test_app_with_models(
    models: Arc<TestModelService>,
    inference: Option<&InferenceLedgerFixture>,
) -> (Router, Arc<tokio::sync::Mutex<HashMap<String, Value>>>) {
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_model_service(models.clone())
        .with_session_service(Arc::new(StubSession));

    let ledger = base.edge_callback_ledger();

    let lifecycle = test_run_lifecycle(
        test_fernet_encryptor("web-e2e-fernet-key-32-chars!!!"),
        ledger.clone(),
    )
    .with_model_service(models)
    .with_auxiliary_event_writer(Arc::new(NoopAuxiliaryEventWriter));
    let lifecycle = if let Some(inference) = inference {
        lifecycle.with_e2e_inference_ledger(inference)
    } else {
        lifecycle
    };

    let state = base.with_run_lifecycle_service(Arc::new(lifecycle));
    (build_app(state), ledger)
}

async fn build_native_test_app(
    scripts: Vec<ProviderScript>,
) -> (Router, ProviderGateway, InferenceLedgerFixture) {
    let gateway = ProviderGateway::start(scripts).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _) = build_test_app_with_models(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );
    (app, gateway, inference)
}

fn primary_request_for(request: &ProviderRequest, message: &str) -> bool {
    request.path == "/v1/chat/completions"
        && request.body["model"] == "test-model"
        && request.body["stream"] == true
        && request.body["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|value| value["role"] == "user" && value["content"] == message)
        })
}

/// Build a test app with recording hook DB + observer writers for verification.
fn build_test_app_with_hooks(
    models: Arc<TestModelService>,
    inference: Option<&InferenceLedgerFixture>,
) -> (
    Router,
    Arc<RecordingHookDbWriter>,
    Arc<RecordingObserverWorker>,
    test_support::EdgeCallbackLedger,
) {
    init_env();
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession));

    let ledger = base.edge_callback_ledger();
    let hook_writer = Arc::new(RecordingHookDbWriter::default());
    let observer_worker = Arc::new(RecordingObserverWorker::default());
    let lifecycle = test_run_lifecycle(
        test_fernet_encryptor("web-e2e-fernet-key-32-chars!!!"),
        ledger.clone(),
    )
    .with_model_service(models)
    .with_hook_db_writer(hook_writer.clone())
    .with_observer_worker(observer_worker.clone())
    .with_auxiliary_event_writer(Arc::new(NoopAuxiliaryEventWriter));

    let lifecycle = if let Some(inference) = inference {
        lifecycle.with_e2e_inference_ledger(inference)
    } else {
        lifecycle
    };
    let state = base.with_run_lifecycle_service(Arc::new(lifecycle));
    (build_app(state), hook_writer, observer_worker, ledger)
}

fn build_test_app_with_hooks_and_skills(
    models: Arc<TestModelService>,
    inference: Option<&InferenceLedgerFixture>,
) -> (
    Router,
    Arc<RecordingHookDbWriter>,
    Arc<RecordingObserverWorker>,
) {
    init_env();
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession));

    let ledger = base.edge_callback_ledger();
    let hook_writer = Arc::new(RecordingHookDbWriter::default());
    let observer_worker = Arc::new(RecordingObserverWorker::default());
    let lifecycle = test_run_lifecycle(
        test_fernet_encryptor("web-e2e-fernet-key-32-chars!!!"),
        ledger,
    )
    .with_model_service(models)
    .with_skill_service(Arc::new(TestSkillService))
    .with_hook_db_writer(hook_writer.clone())
    .with_observer_worker(observer_worker.clone())
    .with_auxiliary_event_writer(Arc::new(NoopAuxiliaryEventWriter));

    let lifecycle = if let Some(inference) = inference {
        lifecycle.with_e2e_inference_ledger(inference)
    } else {
        lifecycle
    };
    let state = base.with_run_lifecycle_service(Arc::new(lifecycle));
    (build_app(state), hook_writer, observer_worker)
}

fn build_test_app_with_agent_bindings(
    binding_service: Arc<InMemoryAgentBindingService>,
    models: Arc<TestModelService>,
    inference: Option<&InferenceLedgerFixture>,
) -> (Router, Arc<RecordingObserverWorker>) {
    init_env();
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession));

    let ledger = base.edge_callback_ledger();
    let observer_worker = Arc::new(RecordingObserverWorker::default());
    let lifecycle = test_run_lifecycle(
        test_fernet_encryptor("web-e2e-fernet-key-32-chars!!!"),
        ledger,
    )
    .with_model_service(models)
    .with_agent_binding_service(binding_service)
    .with_observer_worker(observer_worker.clone())
    .with_auxiliary_event_writer(Arc::new(NoopAuxiliaryEventWriter));

    let lifecycle = if let Some(inference) = inference {
        lifecycle.with_e2e_inference_ledger(inference)
    } else {
        lifecycle
    };
    let state = base.with_run_lifecycle_service(Arc::new(lifecycle));
    (build_app(state), observer_worker)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn delegation_assessment(body: &Value) -> Option<Value> {
    body["messages"].as_array()?.iter().find_map(|message| {
        let input: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
        (input["user_text"].is_string()
            && input["candidates"].is_array()
            && input["slots"].is_array())
        .then_some(input)
    })
}

async fn assert_native_delegation_judgment(
    gateway: &ProviderGateway,
    user_text: &str,
    slots: &[(&str, &str)],
) {
    let requests = gateway.requests.lock().await;
    let judgments = requests
        .iter()
        .filter_map(|request| delegation_assessment(&request.body).map(|input| (request, input)))
        .collect::<Vec<_>>();
    assert_eq!(judgments.len(), 1);
    let (request, input) = &judgments[0];
    assert_eq!(request.body["model"], "test-model");
    assert_eq!(request.body["stream"], true);
    assert_eq!(input["user_text"], user_text);
    let candidates = input["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["offering_id"], DEFAULT_MODEL_OFFERING_ID);
    assert_eq!(candidates[0]["model_name"], "test-model");
    assert_eq!(candidates[0]["provider"], "openai");
    let actual = input["slots"].as_array().unwrap();
    assert_eq!(actual.len(), slots.len());
    for (index, (description, prompt)) in slots.iter().enumerate() {
        assert_eq!(actual[index]["index"], index);
        assert_eq!(actual[index]["description"], *description);
        assert_eq!(actual[index]["prompt"], *prompt);
    }
}

fn assert_child_joined_before_parent(
    events: &[Value],
    agent_id: &Value,
    result: &str,
    parent_text: &str,
) {
    let child_terminal = events
        .iter()
        .position(|event| {
            event["agent_id"] == *agent_id
                && (event["type"] == "agent_completed" || event["event_type"] == "agent_completed")
                && event["result_summary"] == result
        })
        .expect("exact child completed result");
    let parent_final = events
        .iter()
        .position(|event| event["type"] == "text_delta" && event["content"] == parent_text)
        .expect("parent synthesis");
    assert!(
        child_terminal < parent_final,
        "child must join before parent synthesis"
    );
}

fn normalize_chat_stream_payload(mut payload: Value) -> Value {
    let Some(object) = payload.as_object_mut() else {
        return payload;
    };
    if !object.contains_key("model_selection") {
        object.insert(
            "model_selection".to_string(),
            json!({ "offering_id": DEFAULT_MODEL_OFFERING_ID }),
        );
    }
    ensure_test_edge_profile_for_edge_tools(object);
    payload
}

fn ensure_test_edge_profile_for_edge_tools(payload: &mut Map<String, Value>) {
    let Some(context) = payload.get_mut("context").and_then(Value::as_object_mut) else {
        return;
    };
    let has_edge_runtime_tools = context
        .get("edge_tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(test_tool_requires_edge_runtime));
    if !has_edge_runtime_tools {
        return;
    }
    context
        .entry("edge_profile".to_string())
        .or_insert_with(|| {
            json!({
                "cwd": "/tmp/astra-web-agent-e2e-edge",
                "edge_agent_id": "web-agent-e2e-edge",
                "hostname": "web-agent-e2e",
            })
        });
    let workspace_binding = payload
        .entry("workspace_binding".to_string())
        .or_insert_with(|| {
            json!({
                "kind": "edge_workspace",
                "display_name": "web-agent-e2e",
                "root": "/tmp/astra-web-agent-e2e-edge",
                "source": {
                    "kind": "edge_path",
                    "path": "/tmp/astra-web-agent-e2e-edge"
                },
                "authority": "read_write",
            })
        });
    assert_eq!(
        workspace_binding["kind"].as_str(),
        Some("edge_workspace"),
        "test payload with edge runtime tools must use an edge workspace binding"
    );
    let executor_binding = payload
        .entry("executor_binding".to_string())
        .or_insert_with(|| {
            json!({
                "kind": "edge_agent",
                "executor_id": "web-agent-e2e-edge",
                "display_name": "web-agent-e2e",
                "transport": "edge_ledger",
                "status": "online"
            })
        });
    assert_eq!(
        executor_binding["kind"].as_str(),
        Some("edge_agent"),
        "test payload with edge runtime tools must use an edge executor binding"
    );
}

fn test_tool_requires_edge_runtime(tool: &Value) -> bool {
    astra_runtime_env::tool_schema_name(tool).is_some_and(test_tool_uses_client_ledger)
}

fn test_tool_uses_client_ledger(tool_name: &str) -> bool {
    astra_runtime_env::ToolRegistry::builtins()
        .get(tool_name)
        .is_some_and(|spec| {
            matches!(
                spec.required.executor,
                astra_runtime_env::RequiredExecutor::RuntimeExecutor
                    | astra_runtime_env::RequiredExecutor::ServiceOrRuntimeExecutor
            )
        })
}

/// Send a POST /chat/stream request and collect all SSE events from the stream.
async fn chat_stream_collect(app: &Router, payload: Value) -> Vec<Value> {
    let payload = normalize_chat_stream_payload(payload);
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-astra-e2e-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Collect the SSE body. Each line has format: "data: {json}\n\n"
    let body_bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    parse_sse_events(&body_str)
}

async fn provider_chat_stream_collect(app: &Router, payload: Value) -> Vec<Value> {
    let payload = normalize_chat_stream_payload(payload);
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", PROVIDER_TOKEN)
        .header("content-type", "application/json")
        .header("x-astra-e2e-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body_bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    parse_sse_events(&body_str)
}

async fn create_e2e_agent_binding(
    service: &InMemoryAgentBindingService,
    binding_name: &str,
    agent_md: &str,
) -> String {
    AgentBindingService::create_binding(
        service,
        AgentBindingOwnerScope::from_principal(&AuthPrincipal {
            user: AuthUserRecord {
                user_id: format!("provider_authorized:{PROVIDER_ID}:agent-binding-registrar"),
                username: "agent-binding-registrar".to_string(),
                email: String::new(),
                display_name: None,
            },
            session_id: None,
            origin: AuthPrincipalOrigin::ProviderAuthorizedRequest(
                AuthProviderAuthorizedRequestContext {
                    provider_id: PROVIDER_ID.to_string(),
                    external_subject: "agent-binding-registrar".to_string(),
                    provider_scope_id: "agent-binding-registry".to_string(),
                    request_authorization_id: "agent-binding-registration".to_string(),
                    edge_agent_id: None,
                },
            ),
        }),
        AgentBindingCreateRequestData {
            idempotency_key: format!("web-agent-e2e-{binding_name}"),
            binding: AgentBindingPayload {
                binding_name: binding_name.to_string(),
                agent_md: agent_md.to_string(),
                metadata: None,
                binding_schema_version: "v1".to_string(),
            },
        },
    )
    .await
    .expect("create E2E Agent Binding")
    .id
}

#[derive(Clone, Debug)]
struct AgentBindingGatewayCall {
    authorization: String,
    method: String,
    agent_binding_id: Option<String>,
    skill_id: Option<String>,
}

async fn start_agent_binding_gateway(
    foundation_binding_id: String,
    extension_binding_id: String,
) -> (
    String,
    Arc<tokio::sync::Mutex<Vec<AgentBindingGatewayCall>>>,
    tokio::task::JoinHandle<()>,
) {
    let calls = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let handler_calls = calls.clone();
    let handler = move |headers: HeaderMap, Json(body): Json<Value>| {
        let calls = handler_calls.clone();
        let foundation_binding_id = foundation_binding_id.clone();
        let extension_binding_id = extension_binding_id.clone();
        async move {
            let authorization = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let method = body
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let agent_binding_id = body
                .pointer("/params/agent_binding_id")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let skill_id = body
                .pointer("/params/id")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            calls.lock().await.push(AgentBindingGatewayCall {
                authorization,
                method: method.clone(),
                agent_binding_id: agent_binding_id.clone(),
                skill_id: skill_id.clone(),
            });

            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let response = match method.as_str() {
                "tools/list" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"tools": []}
                }),
                "skills/list" if agent_binding_id.as_deref() == Some(&foundation_binding_id) => {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"skills": [{
                            "name": "moi.agent.momo.skill.pdf",
                            "description": "Work with PDF documents"
                        }]}
                    })
                }
                "skills/list" if agent_binding_id.as_deref() == Some(&extension_binding_id) => {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"skills": [{
                            "name": "financial-analysis",
                            "description": "Analyze financial statements"
                        }]}
                    })
                }
                "skills/read"
                    if agent_binding_id.as_deref() == Some(&extension_binding_id)
                        && skill_id.as_deref() == Some("financial-analysis") =>
                {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"skill": {
                            "id": "financial-analysis",
                            "instruction": {
                                "body": "Use the user-provided financial analysis workflow."
                            }
                        }}
                    })
                }
                _ => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": "unexpected binding capability call"}
                }),
            };
            Json(response)
        }
    };
    let app = Router::new().route("/capabilities", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Agent Binding gateway");
    let address = listener
        .local_addr()
        .expect("Agent Binding gateway address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve Agent Binding gateway");
    });
    (format!("http://{address}/capabilities"), calls, server)
}

fn agent_binding_chat_payload(
    foundation_binding_id: &str,
    extension_binding_id: &str,
    capability_endpoint: &str,
    model_endpoint: &str,
) -> Value {
    json!({
        "message": "Analyze the attached financial statement.",
        "stable_runtime_system_prompt": "User-added capabilities take precedence when they are semantically applicable.",
        "model_selection": {"offering_id": DEFAULT_MODEL_OFFERING_ID},
        "resolved_model_selection": {
            "offering_id": DEFAULT_MODEL_OFFERING_ID,
            "model_name": "test-model"
        },
        "agent_bindings": [
            {"id": foundation_binding_id},
            {"id": extension_binding_id}
        ],
        "runtime_auth": {"authorization": "Bearer runtime-grant"},
        "capability_descriptors": {
            "model_gateway": {
                "id": "moi-model-gateway",
                "type": "model_gateway",
                "transport": "http",
                "endpoint_url": model_endpoint,
                "protocol": "openai_chat_completions",
                "model_context_window": 128000
            },
            "mcp": {
                "id": "moi-tools",
                "type": "mcp",
                "transport": "streamable_http",
                "endpoint_url": capability_endpoint,
                "protocol": "mcp"
            },
            "skills": {
                "id": "moi-skills",
                "type": "skills",
                "transport": "streamable_http",
                "endpoint_url": capability_endpoint,
                "protocol": "astra_skills"
            }
        },
        "execution_policy": {"turn_intent": "fixed_default", "skill_auto_route":"disabled"}
    })
}

#[tokio::test]
async fn multi_agent_binding_http_e2e_discovers_and_reads_skill_from_owning_binding() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let binding_service = Arc::new(InMemoryAgentBindingService::new());
        let foundation_binding_id = create_e2e_agent_binding(
            binding_service.as_ref(),
            "momo-foundation-e2e",
            "Follow the platform contract and sandbox safety rules.",
        )
        .await;
        let extension_binding_id = create_e2e_agent_binding(
            binding_service.as_ref(),
            "financial-extension-e2e",
            "Act as the user's financial analyst.",
        )
        .await;
        let (capability_endpoint, calls, gateway_server) = start_agent_binding_gateway(
            foundation_binding_id.clone(),
            extension_binding_id.clone(),
        )
        .await;
        let gateway=ProviderGateway::start(vec![ProviderScript::new("binding skill and actual continuation", |request| primary_request_for(request,"Analyze the attached financial statement."), vec![
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","tool_calls":[tool_call("tc-financial-analysis","skill",json!({"skill_name":"financial-analysis"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Financial analysis completed with the user workflow."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
let inference=InferenceLedgerFixture::default();
let model_endpoint=format!("{}/v1",gateway.base_url);
let (app,observer_worker)=build_test_app_with_agent_bindings(binding_service, Arc::new(TestModelService{judgment_base_url:Some(model_endpoint.clone())}),Some(&inference));

        let events = provider_chat_stream_collect(
            &app,
            agent_binding_chat_payload(
                &foundation_binding_id,
                &extension_binding_id,
                &capability_endpoint,
                &format!("{model_endpoint}/chat/completions"),
            ),
        )
        .await;

        gateway.assert_complete();
        assert!(
            find_events(&events, "error").is_empty(),
            "unexpected SSE error events: {events:?}"
        );
        assert_eq!(find_events(&events, "turn_complete").len(), 1, "native binding events: {events:?}");
        assert!(find_events(&events, "text_delta").iter().any(|event| {
            event["content"]
                .as_str()
                .is_some_and(|content| content.contains("Financial analysis completed"))
        }));

        let ow = observer_worker.clone();
        poll_until(
            || {
                let ow = ow.clone();
                async move { !ow.requests.lock().await.is_empty() }
            },
            5,
        )
        .await;
        let requests = observer_worker.requests.lock().await;
        let skill_result = requests
            .first()
            .expect("observer receives the skill follow-up round")
            .messages
            .iter()
            .find(|message| {
                message.get("tool_call_id").and_then(Value::as_str) == Some("tc-financial-analysis")
            })
            .and_then(|message| message.get("content").and_then(Value::as_str))
            .expect("skill instructions reach the follow-up model round");
        assert!(skill_result.contains("Use the user-provided financial analysis workflow."));
        assert!(skill_result.contains("<skill-loaded name=\"financial-analysis\"/>"));
        drop(requests);

        let calls = calls.lock().await;
        assert!(
            calls
                .iter()
                .all(|call| call.authorization == "Bearer runtime-grant")
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.method == "tools/list")
                .count(),
            1
        );
        let listed_binding_ids = calls
            .iter()
            .filter(|call| call.method == "skills/list")
            .filter_map(|call| call.agent_binding_id.as_deref())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            listed_binding_ids,
            std::collections::HashSet::from([
                foundation_binding_id.as_str(),
                extension_binding_id.as_str(),
            ])
        );
        let read_calls = calls
            .iter()
            .filter(|call| call.method == "skills/read")
            .collect::<Vec<_>>();
        assert_eq!(read_calls.len(), 1);
        assert_eq!(
            read_calls[0].agent_binding_id.as_deref(),
            Some(extension_binding_id.as_str())
        );
        assert_eq!(
            read_calls[0].skill_id.as_deref(),
            Some("financial-analysis")
        );
        drop(calls);
        gateway_server.abort();
        gateway.assert_complete();
        inference.assert_quiescent();
        assert_eq!(inference.attempt_count(),2);
        let wire=gateway.requests.lock().await;
        assert_eq!(wire.len(),2);
        let instructions=wire[1].body["messages"].as_array().unwrap().iter().find(|message| message["tool_call_id"]=="tc-financial-analysis").unwrap()["content"].as_str().unwrap();
        assert!(instructions.contains("Use the user-provided financial analysis workflow."));
        assert!(instructions.contains("<skill-loaded name=\"financial-analysis\"/>"));
    })
    .await
    .expect("multi-Agent-Binding HTTP E2E timed out");
}

#[tokio::test]
async fn multi_agent_binding_http_e2e_reports_exact_missing_binding_id() {
    let binding_service = Arc::new(InMemoryAgentBindingService::new());
    let foundation_binding_id = create_e2e_agent_binding(
        binding_service.as_ref(),
        "momo-foundation-missing-binding-e2e",
        "Follow the platform contract and sandbox safety rules.",
    )
    .await;
    let missing_binding_id = "ab_missing_financial_extension";
    let (app, _observer_worker) = build_test_app_with_agent_bindings(
        binding_service,
        Arc::new(TestModelService::default()),
        None,
    );

    let events = provider_chat_stream_collect(
        &app,
        agent_binding_chat_payload(
            &foundation_binding_id,
            missing_binding_id,
            "http://127.0.0.1:9/capabilities",
            "http://127.0.0.1:9/v1",
        ),
    )
    .await;

    let error = find_events(&events, "error")
        .into_iter()
        .next()
        .expect("missing binding produces an SSE error");
    assert_eq!(
        error["error_code"].as_str(),
        Some("agent_binding_not_found")
    );
    assert_eq!(error["agent_binding_id"].as_str(), Some(missing_binding_id));
    assert!(find_events(&events, "session_info").is_empty());
}

/// Send a POST /chat/stream and return the streaming body as a stream of bytes.
/// This is used for tests that need to read events incrementally while posting
/// tool results concurrently.
async fn chat_stream_start(app: &Router, payload: Value) -> axum::response::Response {
    let payload = normalize_chat_stream_payload(payload);
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-astra-e2e-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn post_unmatched_tool_result(
    app: &Router,
    request_id: &str,
    output: &str,
    status: &str,
) -> StatusCode {
    post_tool_result_with_identity(
        app,
        request_id,
        output,
        status,
        unmatched_tool_result_identity(),
    )
    .await
}

async fn post_tool_result_from_event(
    app: &Router,
    event: &Value,
    output: &str,
    status: &str,
) -> StatusCode {
    let (request_id, identity) =
        tool_request_identity_from_event(event).expect("tool_request event must carry identity");
    post_tool_result_with_identity(app, &request_id, output, status, identity).await
}

async fn post_tool_result_with_identity(
    app: &Router,
    request_id: &str,
    output: &str,
    status: &str,
    identity: ToolResultIdentity,
) -> StatusCode {
    let body = astra_thin_client::ToolResultRequest::new_with_hash(
        astra_thin_client::ToolResultRequestParts {
            session_id: identity.session_id,
            run_id: identity.run_id,
            turn_chain_id: identity.turn_chain_id,
            request_id: request_id.to_string(),
            edge_agent_id: DEFAULT_TEST_EDGE_AGENT_ID.to_string(),
            status: status.to_string(),
            output: output.to_string(),
            duration_ms: 10,
            tool_result_fields: None,
        },
    );
    let body: Value = serde_json::to_value(body).unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/tools/result")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let response_status = response.status();
    if !response_status.is_success() {
        let response_body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("tool result error response body");
        eprintln!(
            "tool result callback failed: status={response_status}, body={}",
            String::from_utf8_lossy(&response_body)
        );
    }
    response_status
}

#[derive(Debug, Clone)]
struct ApprovalIdentity {
    session_id: String,
    run_id: String,
}

async fn wait_for_approval_identity(rx: &mut mpsc::UnboundedReceiver<Value>) -> ApprovalIdentity {
    let session_info = wait_for_sse(rx, "session_info", E2E_WAIT_TIMEOUT_SECS).await;
    ApprovalIdentity {
        session_id: session_info
            .get("session_id")
            .and_then(Value::as_str)
            .expect("session_info.session_id")
            .to_string(),
        run_id: session_info
            .get("run_id")
            .and_then(Value::as_str)
            .expect("session_info.run_id")
            .to_string(),
    }
}

/// POST /approval/respond
async fn post_approval_respond(
    app: &Router,
    identity: &ApprovalIdentity,
    request_id: &str,
    decision: &str,
) -> StatusCode {
    post_approval_respond_with_body(app, identity, request_id, decision)
        .await
        .0
}

async fn post_approval_respond_with_body(
    app: &Router,
    identity: &ApprovalIdentity,
    request_id: &str,
    decision: &str,
) -> (StatusCode, String) {
    let body = json!({
        "request_id": request_id,
        "decision": decision,
        "session_id": identity.session_id,
        "run_id": identity.run_id,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/approval/respond")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("approval response body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Read SSE events incrementally from a streaming response body.
async fn read_sse_events_from_body(body: Body) -> Vec<Value> {
    let bytes = body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&bytes);
    parse_sse_events(&body_str)
}

async fn cancel_run(app: &Router, run_id: &str) -> StatusCode {
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/chat/runs/{run_id}"))
        .header("authorization", TOKEN)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> Option<&'a Value> {
    events
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some(event_type))
}

fn find_events<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some(event_type))
        .collect()
}

fn find_event_type<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| {
            e.get("type").and_then(Value::as_str) == Some(event_type)
                || e.get("event_type").and_then(Value::as_str) == Some(event_type)
        })
        .collect()
}

#[tokio::test]
async fn web_agent_structured_spawn_waits_for_server_child_before_parent_synthesis() {
    structured_spawn_journey(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_web_agent_propagates_execution_owner_to_real_child() {
    let settings = test_support::require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    astra_services::ensure_core_schema(&settings, &catalog)
        .await
        .unwrap();
    let pool = astra_core::SharedPool::new(&settings).await.unwrap();
    structured_spawn_journey(Some(pool)).await;
}

#[tokio::test]
async fn manual_pause_discards_losing_evaluation_from_live_and_replay() {
    paused_evaluation_journey(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires disposable MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_manual_pause_discards_losing_evaluation_from_live_and_replay() {
    let settings = test_support::require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    astra_services::ensure_core_schema(&settings, &catalog)
        .await
        .unwrap();
    paused_evaluation_journey(Some(astra_core::SharedPool::new(&settings).await.unwrap())).await;
}

async fn paused_evaluation_journey(pool: Option<astra_core::SharedPool>) {
    use futures_util::FutureExt;
    init_env();
    const MESSAGE: &str = "Explain the completed result after this request.";
    let session = format!("paused-evaluation-{}", uuid::Uuid::new_v4());
    let release = Arc::new(tokio::sync::Notify::new());
    let outcome = std::panic::AssertUnwindSafe(async {
        let gateway = ProviderGateway::start(vec![ProviderScript::new(
            "pause before final provider response", |request| primary_request_for(request, MESSAGE),
            vec![ProviderResponse::Stream {
                content_type: "text/event-stream",
                chunks: vec![
                    format!("data: {}\n\n", json!({"id":"pause-native","model":"test-model","choices":[{"index":0,"delta":{"content":"The result is ready."},"finish_reason":null}]})).into_bytes(),
                    format!("data: {}\n\n", json!({"id":"pause-native","model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})).into_bytes(),
                    b"data: [DONE]\n\n".to_vec(),
                ],
                release_before_chunk: Some((1, release.clone())),
            }],
        )]).await;
        let models = Arc::new(TestModelService { judgment_base_url: Some(format!("{}/v1", gateway.base_url)) });
        let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
            .with_auth_service(Arc::new(StubAuth)).with_session_service(Arc::new(StubSession));
        let store: Arc<dyn astra_services::RunStateStore> = if let Some(pool) = &pool {
            sqlx::query("INSERT INTO agent_sessions (session_id,user_id,agent_id,title,status,metadata,created_at,updated_at) VALUES (?,?,'run-test-agent','pause evaluation','active','{}',NOW(6),NOW(6))")
                .bind(&session).bind(USER_ID).execute(pool.get()).await.unwrap();
            Arc::new(astra_services::DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("pause-evaluation-owner"))
        } else { Arc::new(astra_services::InMemoryRunStateStore::new()) };
        let engine = astra_runtime::RunEngine::new(store);
        let inference = InferenceLedgerFixture::default();
        let lifecycle = astra_runtime::AgenticRunLifecycleService::new(
            astra_runtime::MatrixOneSettings::mock(), test_fernet_encryptor("web-e2e-fernet-key-32-chars!!!"),
            base.edge_callback_ledger(), engine.clone());
        let lifecycle = astra_runtime::server::provider_test_support::configure_workspace_provider(
            lifecycle, Arc::new(tempfile::tempdir().unwrap()), "pause-evaluation-executor")
            .with_model_service(models).with_e2e_inference_ledger(&inference)
            .with_auxiliary_event_writer(Arc::new(NoopAuxiliaryEventWriter));
        let lifecycle = if let Some(pool) = &pool { lifecycle.with_pool(pool.clone()) } else { lifecycle };
        let app = build_app(base.with_run_lifecycle_service(Arc::new(lifecycle)));
        let response = chat_stream_start(&app, json!({"message":MESSAGE,"session_id":session,
            "model_selection":{"offering_id":DEFAULT_MODEL_OFFERING_ID},
            "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"}})).await;
        let (mut rx, reader) = spawn_sse_reader(response.into_body()).await;
        let identity = wait_for_sse(&mut rx, "session_info", E2E_WAIT_TIMEOUT_SECS).await;
        let run_id = identity["run_id"].as_str().unwrap();
        wait_for_sse(&mut rx, "text_delta", E2E_WAIT_TIMEOUT_SECS).await;
        let request = Request::builder().method("POST").uri(format!("/chat/runs/{run_id}/pause"))
            .header("authorization", TOKEN).body(Body::empty()).unwrap();
        assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::OK);
        release.notify_one();
        let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader).await.unwrap().unwrap();
        assert!(events.iter().all(|event| event.get("turn_evaluation").is_none()), "losing live candidate: {events:?}");
        let durable = engine.load_run(USER_ID, run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, "paused");
        assert!(durable.events.iter().all(|event| event.pointer("/data/turn_evaluation").is_none()), "losing durable candidate: {:?}", durable.events);
        let (status, replay) = get_run_stream(&app, run_id, 0).await;
        assert_eq!(status, StatusCode::OK);
        assert!(replay.iter().all(|event| event.get("turn_evaluation").is_none()), "losing process-local replay: {replay:?}");
        let replay_lifecycle = astra_runtime::AgenticRunLifecycleService::new(
            astra_runtime::MatrixOneSettings::mock(), test_fernet_encryptor("web-e2e-fernet-key-32-chars!!!"),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())), engine);
        let replay_app = build_app(AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
            .with_auth_service(Arc::new(StubAuth)).with_session_service(Arc::new(StubSession))
            .with_run_lifecycle_service(Arc::new(replay_lifecycle)));
        let (status, replay) = get_run_stream(&replay_app, run_id, 0).await;
        assert_eq!(status, StatusCode::OK);
        assert!(replay.iter().all(|event| event.get("turn_evaluation").is_none()), "losing durable replay: {replay:?}");
        gateway.assert_complete();
        inference.assert_quiescent();
    }).catch_unwind().await;
    release.notify_one();
    if let Some(pool) = &pool {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id=? AND session_id=? AND owner_pod_id IS NOT NULL")
                    .bind(USER_ID).bind(&session).fetch_one(pool.get()).await.unwrap();
                if active == 0 { break; }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }).await.expect("paused fixture owners must drain before cleanup");
        for table in [
            "tool_invocation_ledger",
            "agent_session_execution_slots",
            "run_display_projections",
            "run_checkpoints",
            "agent_run_events",
            "agent_runs",
            "agent_sessions",
        ] {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE user_id=? AND session_id=?"
                ))
                .bind(USER_ID)
                .bind(&session)
                .execute(pool.get()),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}

async fn structured_spawn_journey(pool: Option<astra_core::SharedPool>) {
    use futures_util::FutureExt;
    let session = format!("native-child-owner-{}", uuid::Uuid::new_v4());
    let release_child = Arc::new(tokio::sync::Notify::new());
    let result = std::panic::AssertUnwindSafe(async {
    init_env();
    use astra_runtime::server::provider_test_support::{
        InferenceLedgerFixture, ProviderGateway, ProviderResponse, ProviderScript,
    };
    const ROOT: &str = "Use a child agent to review the code.";
    const TASK: &str = "Review src/lib.rs and summarize one issue.";
    fn assessment(body: &Value) -> Option<Value> {
        body["messages"].as_array()?.iter().find_map(|message| {
            let input: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
            (input["user_text"].is_string()
                && input["candidates"].is_array()
                && input["slots"].is_array())
            .then_some(input)
        })
    }
    fn primary(text: &str, tools: Vec<Value>) -> ProviderResponse {
        let finish = if tools.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        ProviderResponse::OpenAi(json!({
            "id":format!("native-web-{}",uuid::Uuid::new_v4()),"model":"test-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":tools},"finish_reason":finish}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
        }))
    }
    fn root_request(body: &Value) -> bool {
        assessment(body).is_none()
            && body["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message["role"] == "user" && message["content"] == ROOT)
            })
    }
    let child_entered = Arc::new(tokio::sync::Notify::new());
    let child_response = primary("child review result: no critical issues", Vec::new());
    let child_response = {
        let ProviderResponse::OpenAi(response) = child_response else {
            unreachable!()
        };
        let chunk = json!({"id":response["id"],"model":response["model"],
            "choices":[{"index":0,"delta":{"content":"child review result: no critical issues"},"finish_reason":null}]});
        let finish = json!({"id":response["id"],"model":response["model"],
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":response["usage"]});
        ProviderResponse::Stream {
            content_type: "text/event-stream",
            chunks: vec![
                format!("data: {chunk}\n\n").into_bytes(),
                format!("data: {finish}\n\n").into_bytes(),
                b"data: [DONE]\n\n".to_vec(),
            ],
            release_before_chunk: Some((0, release_child.clone())),
        }
    };
    let entered = child_entered.clone();
    let gateway = ProviderGateway::start(vec![
        ProviderScript::new("root discovery, spawn, wait, synthesis", |request|
            request.path == "/v1/chat/completions" && request.body["model"] == "test-model" && root_request(&request.body), vec![
            primary("", vec![tool_call("call-select-agent", "tool_search", json!({"query":"select:agent"}))]),
            primary("", vec![tool_call("call-spawn-reviewer", "invoke_tool", json!({"name":"agent","arguments":{
                "action":"spawn","description":"structured child review","prompt":TASK,"agent_type":"code-review"
            }}))]),
            primary("", vec![tool_call("call-join-reviewer", "invoke_tool", json!({"name":"agent","arguments":{"action":"wait","timeout_ms":10000}}))]),
            primary("parent synthesis grounded in child review", Vec::new()),
        ]),
        ProviderScript::new("actual delegated child", move |request| {
            let matches = request.path == "/v1/chat/completions" && request.body["model"] == "test-model" && assessment(&request.body).is_none()
                && !root_request(&request.body) && request.body["messages"].as_array().is_some_and(|messages|
                    messages.iter().any(|message| message["content"].as_str().is_some_and(|text| text.contains(TASK))));
            if matches { entered.notify_one(); }
            matches
        }, vec![child_response]),
        ProviderScript::new("actual delegation judgment", |request|
            request.path == "/v1/chat/completions" && request.body["model"] == "test-model" && assessment(&request.body).is_some(),
            vec![ProviderResponse::OpenAi(json!({"id":"native-web-judgment","model":"test-model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"{\"disposition\":\"not_applicable\"}"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":32,"completion_tokens":8,"total_tokens":40}
            }))]),
    ]).await;
    let models = Arc::new(TestModelService {
        judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
    });
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_model_service(models.clone())
        .with_session_service(Arc::new(StubSession));
    let store: Arc<dyn astra_services::RunStateStore> = if let Some(pool) = &pool {
        sqlx::query("INSERT INTO agent_sessions (session_id,user_id,agent_id,title,status,metadata,created_at,updated_at) VALUES (?,?,'run-test-agent','native child owner','active','{}',NOW(6),NOW(6))")
            .bind(&session).bind(USER_ID).execute(pool.get()).await.unwrap();
        Arc::new(
            astra_services::DatabaseRunStateStore::new(pool.clone())
                .with_owner_pod_id("web-parent-owner"),
        )
    } else {
        Arc::new(astra_services::InMemoryRunStateStore::new())
    };
    let engine = astra_runtime::RunEngine::new(store);
    let inference = InferenceLedgerFixture::default();
    let lifecycle = astra_runtime::AgenticRunLifecycleService::new(
        astra_runtime::MatrixOneSettings::mock(),
        test_fernet_encryptor("web-e2e-fernet-key-32-chars!!!"),
        base.edge_callback_ledger(),
        engine.clone(),
    )
    ;
    let workspace_directory = Arc::new(tempfile::tempdir().unwrap());
    let directory_lifetime = Arc::downgrade(&workspace_directory);
    let workspace_base = workspace_directory.path().to_owned();
    let lifecycle = astra_runtime::server::provider_test_support::configure_workspace_provider(
        lifecycle, workspace_directory.clone(), "structured-spawn-fixture-executor")
    .with_model_service(models)
    .with_e2e_inference_ledger(&inference)
    .with_auxiliary_event_writer(Arc::new(NoopAuxiliaryEventWriter));
    let lifecycle = if let Some(pool) = &pool {
        lifecycle.with_pool(pool.clone())
    } else {
        lifecycle
    };
    let managed_record = astra_runtime::server::provider_test_support::provision_workspace_fixture(&lifecycle, &session).unwrap();
    let marker = std::path::Path::new(&managed_record.root_or_volume_ref).join("lifetime.txt");
    std::fs::write(&marker, "retained while child executes").unwrap();
    let app = build_app(base.with_run_lifecycle_service(Arc::new(lifecycle)));
    let response = chat_stream_start(
        &app,
        json!({
            "message":ROOT,"session_id":session,"model_selection":{"offering_id":DEFAULT_MODEL_OFFERING_ID},
            "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        }),
    ).await;
    let (_rx, stream) = spawn_sse_reader(response.into_body()).await;
    drop(app);
    drop(workspace_directory);
    tokio::pin!(stream);
    {
        tokio::select! {
            entered = tokio::time::timeout(std::time::Duration::from_secs(30), child_entered.notified()) => { entered.expect("child must reach provider within the admission budget"); }
            events = &mut stream => panic!("child must reach the actual provider before parent finishes: {events:?}"),
        }
        assert!(directory_lifetime.upgrade().is_some(), "actual root/child executor owns the directory after App and caller drop");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "retained while child executes");
    }
    if let Some(pool) = &pool {
        let owners = async {
            let runs: Vec<String> = sqlx::query_scalar(
                "SELECT run_id FROM agent_runs WHERE user_id=? AND session_id=?",
            )
            .bind(USER_ID)
            .bind(&session)
            .fetch_all(pool.get())
            .await
            .map_err(|error| error.to_string())?;
            let mut records = Vec::new();
            for run in runs {
                records.push(
                    engine
                        .load_run(USER_ID, &run)
                        .await?
                        .ok_or_else(|| "durable run missing".to_string())?,
                );
            }
            Ok::<_, String>(records)
        }
        .await;
        release_child.notify_one();
        let owners = owners.unwrap();
        assert_eq!(owners.len(), 2, "root and child must both own durable runs");
        for record in owners {
            assert_eq!(record.owner_pod_id.as_deref(), Some("web-parent-owner"));
        }
    } else {
        release_child.notify_one();
    }
    let events = tokio::time::timeout(std::time::Duration::from_secs(30), stream)
        .await.unwrap().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while directory_lifetime.upgrade().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("workspace fixture must be released after real execution drain");
    assert!(!workspace_base.exists());
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 6);
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 6);
    let judgment = requests
        .iter()
        .filter_map(|request| assessment(&request.body))
        .collect::<Vec<_>>();
    assert_eq!(judgment.len(), 1);
    assert_eq!(judgment[0]["user_text"], ROOT);
    let candidates = judgment[0]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["offering_id"], DEFAULT_MODEL_OFFERING_ID);
    assert_eq!(candidates[0]["model_name"], "test-model");
    assert_eq!(candidates[0]["provider"], "openai");
    let slots = judgment[0]["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0]["index"], 0);
    assert_eq!(slots[0]["description"], "structured child review");
    assert_eq!(slots[0]["prompt"], TASK);
    let judgment_wire = requests
        .iter()
        .find(|request| assessment(&request.body).is_some())
        .unwrap();
    assert_eq!(judgment_wire.body["model"], "test-model");
    assert_eq!(judgment_wire.body["stream"], true);
    let child_wire = requests
        .iter()
        .find(|request| assessment(&request.body).is_none() && !root_request(&request.body))
        .unwrap();
    assert!(
        !child_wire.body["messages"].to_string().contains(ROOT),
        "parent human remains isolated"
    );
    drop(requests);

    assert!(
        find_events(&events, "text_delta")
            .iter()
            .any(|event| event["content"].as_str()
                == Some("parent synthesis grounded in child review")),
        "parent should synthesize after the structured child result: {events:?}"
    );
    let run_id = events[0]
        .get("run_id")
        .and_then(Value::as_str)
        .expect("session_info should include run_id")
        .to_string();
    let serialized = serde_json::to_string(&events).unwrap();
    let launch_receipt = find_events(&events, "tool_call_end")
        .into_iter()
        .find(|event| event["call_id"].as_str() == Some("call-spawn-reviewer"))
        .and_then(|event| event["result"].as_str())
        .and_then(|result| serde_json::from_str::<Value>(result).ok())
        .unwrap_or_else(|| panic!("spawn should return a canonical launch receipt: {serialized}"));
    assert_eq!(launch_receipt["status"], "launched", "{serialized}");
    assert_eq!(launch_receipt["result_family"], "control_receipt");
    assert_eq!(launch_receipt["success"], true);
    let child_run = launch_receipt["run_id"].as_str().unwrap();
    let root_record = engine.load_run(USER_ID, &run_id).await.unwrap().unwrap();
    let child_record = engine.load_run(USER_ID, child_run).await.unwrap().unwrap();
    assert_ne!(root_record.run_id, child_record.run_id);
    assert_eq!(child_record.parent_run_id.as_deref(), Some(run_id.as_str()));
    let admissions = inference.admissions();
    let root_admissions: Vec<_> = admissions
        .iter()
        .filter(|(scope, _)| {
            scope.run_id() == Some(run_id.as_str()) && scope.operation_id() == "agent_turn"
        })
        .collect();
    let child_admissions: Vec<_> = admissions
        .iter()
        .filter(|(scope, _)| scope.run_id() == Some(child_run))
        .collect();
    assert_eq!(root_admissions.len(), 4);
    assert_eq!(child_admissions.len(), 1);
    for (scope, authority) in root_admissions.into_iter().chain(child_admissions) {
        assert_eq!(scope.session_id(), Some(root_record.session_id.as_str()));
        let record = if scope.run_id() == Some(child_run) {
            &child_record
        } else {
            &root_record
        };
        let authority = authority.as_ref().unwrap();
        assert_eq!(authority.expected_owner_generation, record.run_generation);
        assert_eq!(
            authority.expected_owner_pod_id,
            if pool.is_some() {
                "web-parent-owner"
            } else {
                "test-inference-owner"
            },
            "inference must preserve the selected execution owner's pod"
        );
        assert!(
            authority.expected_control_epoch >= 0
                && authority.expected_control_epoch <= record.last_event_idx
        );
    }

    assert_child_joined_before_parent(
        &events,
        &launch_receipt["agent_id"],
        "child review result: no critical issues",
        "parent synthesis grounded in child review",
    );
    assert!(
        launch_receipt["agent_id"].as_str().is_some(),
        "{serialized}"
    );
    assert!(launch_receipt["run_id"].as_str().is_some(), "{serialized}");
    assert!(
        serialized.contains("child review result: no critical issues"),
        "the structured child result should remain visible in the stream: {serialized}"
    );
    let live_events = find_events(&events, "agent_live_event");
    let live_output = live_events
        .iter()
        .find(|event| {
            event["event_kind"].as_str() == Some("output_delta")
                && event["content"].as_str() == Some("child review result: no critical issues")
        })
        .unwrap_or_else(|| {
            panic!("server dynamic spawn should stream child output into agent_live_event: {serialized}")
        });
    assert!(
        live_output["workspace"]["kind"].as_str() == Some("none")
            && live_output["executor"]["kind"].as_str() == Some("server_local")
            && live_output["executor"]["executor_id"].as_str() == Some("server-control-plane")
            && live_output["transport"].as_str() == Some("server_local"),
        "server-only dynamic spawn should stream child output without inventing a workspace executor provider: {serialized}"
    );
    assert!(
        live_events.iter().any(|event| {
            event["event_kind"].as_str() == Some("agent_terminated")
                && event["termination"].as_str() == Some("completed")
        }),
        "server dynamic spawn should stream child terminal live event: {serialized}"
    );
    let child_terminal_index = events
        .iter()
        .position(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("agent_completed")
                || event.get("type").and_then(Value::as_str) == Some("agent_completed")
        })
        .expect("child terminal projection");
    let parent_synthesis_index = events
        .iter()
        .position(|event| {
            event.get("type").and_then(Value::as_str) == Some("text_delta")
                && event.get("content").and_then(Value::as_str)
                    == Some("parent synthesis grounded in child review")
        })
        .expect("parent synthesis");
    assert!(
        child_terminal_index < parent_synthesis_index,
        "the parent must not receive a model boundary before its child is terminal: {serialized}"
    );
    let spawned = find_event_type(&events, "agent_spawned");
    assert!(
        !spawned.is_empty(),
        "server dynamic spawn should emit agent_spawned progress: {serialized}"
    );
    assert_eq!(spawned[0]["workspace"]["kind"], "none");
    assert_eq!(spawned[0]["executor"]["kind"], "server_local");
    assert_eq!(
        spawned[0]["executor"]["executor_id"],
        "server-control-plane"
    );
    assert_eq!(spawned[0]["transport"], "server_local");
    let completed = find_event_type(&events, "agent_completed");
    assert!(
        !completed.is_empty(),
        "server dynamic spawn should emit agent_completed progress: {serialized}"
    );
    assert_eq!(completed[0]["workspace"]["kind"], "none");
    assert_eq!(completed[0]["executor"]["kind"], "server_local");
    assert_eq!(
        completed[0]["executor"]["executor_id"],
        "server-control-plane"
    );

    // Replay uses the same authoritative Run store with a fresh read-only
    // lifecycle, so it cannot keep the original directory capability alive.
    let replay_lifecycle = astra_runtime::AgenticRunLifecycleService::new(
        test_support::test_matrixone_settings(), test_fernet_encryptor("web-e2e-fernet-key-32-chars!!!"),
        Arc::new(tokio::sync::Mutex::new(HashMap::new())), engine.clone());
    let replay_lifecycle = if let Some(pool) = &pool { replay_lifecycle.with_pool(pool.clone()) } else { replay_lifecycle };
    let replay_app = build_app(AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth)).with_session_service(Arc::new(StubSession))
        .with_run_lifecycle_service(Arc::new(replay_lifecycle)));
    let (replay_status, replay_events) = get_run_stream(&replay_app, &run_id, 0).await;
    assert_eq!(replay_status, StatusCode::OK);
    let replay_serialized = serde_json::to_string(&replay_events).unwrap();
    let live_terminal = events.iter().find(|event| event["type"] == "run_finished" && event["run_id"] == run_id).expect("root terminal");
    let replay_terminal = replay_events.iter().find(|event| event["type"] == "run_finished" && event["run_id"] == run_id).expect("replayed root terminal");
    assert_eq!(live_terminal["turn_evaluation"], replay_terminal["turn_evaluation"]);
    let evaluation = astra_turn_core::evaluation::turn_evaluation_from_terminal(
        &live_terminal["turn_evaluation"], Some(&session), &run_id,
        live_terminal["owner_generation"].as_u64(), live_terminal["status"].as_str().unwrap(),
    ).expect("real root evaluation must carry exact authority");
    assert_eq!(evaluation.producer_scope.unwrap().run_id, run_id);
    let child_evaluation = child_record.events.iter().find_map(|event| event.pointer("/data/turn_evaluation")).expect("real child terminal evaluation");
    assert_eq!(child_evaluation["producer_scope"]["run_id"], child_run);
    assert_eq!(child_evaluation["metadata"]["execution_owner_generation"], child_record.run_generation);
    astra_turn_core::evaluation::turn_evaluation_from_terminal(
        child_evaluation, Some(&session), child_run, Some(child_record.run_generation), &child_record.status,
    ).expect("real child evaluation must carry exact authority");

    assert!(
        !find_event_type(&replay_events, "agent_spawned").is_empty(),
        "completed run replay should include durable agent_spawned: {replay_serialized}"
    );
    assert!(
        !find_event_type(&replay_events, "agent_completed").is_empty(),
        "completed run replay should include durable agent_completed: {replay_serialized}"
    );
    }).catch_unwind().await;
    release_child.notify_one();
    if let Some(pool) = &pool {
        // Drain execution owners before deleting this disposable fixture, even
        // when an assertion failed while the child response was gated.
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id=? AND session_id=? AND owner_pod_id IS NOT NULL")
                    .bind(USER_ID).bind(&session).fetch_one(pool.get()).await.unwrap();
                if active == 0 { break; }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }).await.expect("test execution owners must drain before fixture cleanup");
        for table in [
            "tool_invocation_ledger",
            "agent_session_execution_slots",
            "run_display_projections",
            "run_checkpoints",
            "agent_run_events",
            "agent_runs",
            "agent_sessions",
        ] {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE user_id=? AND session_id=?"
                ))
                .bind(USER_ID)
                .bind(&session)
                .execute(pool.get()),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
async fn web_agent_parallel_fanout_without_auxiliary_admission_uses_typed_carrier() {
    init_env();
    let (app,gateway,inference)=build_native_test_app(vec![ProviderScript::new("parent actual execution", |request| primary_request_for(request,"Use two independent child agents and combine their findings.") && delegation_assessment(&request.body).is_none(),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                            tool_call(
                                "call-select-agent-parallel",
                                "tool_search",
                                json!({"query": "select:agent_fanout"})
                            )
                        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                            tool_call("call-fanout", "invoke_tool", json!({
                                "name": "agent_fanout",
                                "arguments": {
                                    "action": "start",
                                    "target_count": 2,
                                    "slots": [
                                        {
                                            "id": "concern-a",
                                            "description": "parallel child A",
                                            "prompt": "Review one independent concern.",
                                            "agent_type": "code-review"
                                        },
                                        {
                                            "id": "concern-b",
                                            "description": "parallel child B",
                                            "prompt": "Review another independent concern.",
                                            "agent_type": "code-review"
                                        }
                                    ]
                                }
                            }))
                        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("call-join-fanout","invoke_tool",json!({"name":"agent","arguments":{"action":"wait","timeout_ms":10000}}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"combined findings from both children","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))]),ProviderScript::new("delegated child actual execution", |request| request.path=="/v1/chat/completions" && request.body["model"]=="test-model" && delegation_assessment(&request.body).is_none() && !primary_request_for(request,"Use two independent child agents and combine their findings."),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"child review completed","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"child review completed","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))]),ProviderScript::new("canonical delegation assessment", |request| request.path=="/v1/chat/completions" && delegation_assessment(&request.body).is_some(),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"{\"disposition\":\"not_applicable\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":32,"completion_tokens":8,"total_tokens":40}}))])]).await;

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "Use two independent child agents and combine their findings.",
            "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
            "context": {

            }
        }),
    )
    .await;
    assert_native_delegation_judgment(
        &gateway,
        "Use two independent child agents and combine their findings.",
        &[
            ("parallel child A", "Review one independent concern."),
            ("parallel child B", "Review another independent concern."),
        ],
    )
    .await;

    let result = find_events(&events, "tool_call_end")
        .into_iter()
        .find(|event| event["call_id"].as_str() == Some("call-fanout"))
        .and_then(|event| event["result"].as_str())
        .and_then(|result| serde_json::from_str::<Value>(result).ok())
        .expect("fanout must return structured launch receipts");
    // No separate auxiliary Offering is configured. The admitted primary
    // route still performs the canonical candidate judgment through HTTP.
    assert_eq!(result["status"], "started");
    let agents = result["agents"]
        .as_array()
        .expect("per-child launch receipts");
    assert_eq!(agents.len(), 2);
    for agent in agents {
        assert_eq!(agent["status"], "launched");
        assert!(agent["agent_id"].as_str().is_some());
        assert!(agent["run_id"].as_str().is_some());
    }
    assert!(
        find_events(&events, "text_delta")
            .iter()
            .any(|event| event["content"].as_str() == Some("combined findings from both children")),
        "the parent should receive the completed fanout and combine its findings"
    );
    let spawned = find_event_type(&events, "agent_spawned");
    assert_eq!(spawned.len(), 2);
    assert_eq!(find_event_type(&events, "agent_completed").len(), 2);
    for child in spawned {
        assert_child_joined_before_parent(
            &events,
            &child["agent_id"],
            "child review completed",
            "combined findings from both children",
        );
        assert_eq!(child["workspace"]["kind"], "none");
        assert_eq!(child["executor"]["kind"], "server_local");
    }

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(gateway.requests.lock().await.len(), 7);
    assert_eq!(inference.attempt_count(), 7);
}

#[tokio::test]
async fn web_agent_parallel_direct_spawns_with_invalid_assessment_fail_closed() {
    init_env();

    let calls: Vec<Value> = ["direct-a", "direct-b"]
        .into_iter()
        .map(|id| {
            tool_call(
                id,
                "invoke_tool",
                json!({
                    "name": "agent",
                    "arguments": {
                        "action": "spawn",
                        "description": format!("Independent review {id}"),
                        "agent_type": "code-review",
                        "prompt": "Review one independent concern."
                    }
                }),
            )
        })
        .collect();
    let invalid_assessment = json!({"choices":[{"index":0,"message":{"role":"assistant","content":"invalid assessment"},"finish_reason":"stop"}],"usage":{"prompt_tokens":32,"completion_tokens":8,"total_tokens":40}});
    let (app,gateway,inference)=build_native_test_app(vec![ProviderScript::new("parent actual execution", |request| primary_request_for(request,"Use two independent child agents and combine their findings.") && delegation_assessment(&request.body).is_none(),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("select-agent", "tool_search", json!({"query": "select:agent"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":calls},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Continuing without unauthorized parallel children.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))]),ProviderScript::new("canonical delegation assessment", |request| request.path=="/v1/chat/completions" && delegation_assessment(&request.body).is_some(),(0..2).map(|_| ProviderResponse::OpenAi(invalid_assessment.clone())).collect())]).await;
    let events = chat_stream_collect(&app, json!({
        "message": "Use two independent child agents and combine their findings.","execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "context": {

        }
    })).await;
    for id in ["direct-a", "direct-b"] {
        let result = find_events(&events, "tool_call_end")
            .into_iter()
            .find(|event| event["call_id"].as_str() == Some(id))
            .and_then(|event| event["result"].as_str())
            .and_then(|result| serde_json::from_str::<Value>(result).ok())
            .expect("direct spawn must return a structured rejection");
        assert_eq!(result["status"], "failed");
        assert_eq!(
            result["error_kind"],
            "delegation_model_assessment_unavailable"
        );
        assert_eq!(result["advisory"]["executed"], false);
    }
    assert!(find_event_type(&events, "agent_spawned").is_empty());
    assert!(find_event_type(&events, "agent_completed").is_empty());
    assert!(find_events(&events, "agent_live_event").is_empty());
    assert!(find_events(&events, "text_delta").iter().all(|event| {
        !event["content"]
            .as_str()
            .unwrap_or_default()
            .contains("must not execute")
    }));

    gateway.assert_complete();
    inference.assert_quiescent();
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests
            .iter()
            .filter(|request| delegation_assessment(&request.body).is_some())
            .count(),
        2,
        "the whole spawn batch shares one assessment and one bounded repair"
    );
    assert_eq!(inference.attempt_count(), 5);
}

#[tokio::test]
async fn web_agent_dynamic_spawn_inherits_edge_workspace_binding() {
    init_env();
    let (app,gateway,inference)=build_native_test_app(vec![ProviderScript::new("parent actual execution", |request| primary_request_for(request,"Use a child agent to review the edge workspace.") && delegation_assessment(&request.body).is_none(),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                            tool_call(
                                "call-select-edge-agent",
                                "tool_search",
                                json!({"query": "select:agent"})
                            )
                        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                            tool_call("call-spawn-edge-reviewer", "invoke_tool", json!({
                                "name": "agent",
                                "arguments": {
                                    "action": "spawn",
                                    "description": "edge child review",
                                    "prompt": "Review src/lib.rs in the inherited edge workspace.",
                                    "agent_type": "code-review"
                                }
                            }))
                        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("call-join-edge-reviewer","invoke_tool",json!({"name":"agent","arguments":{"action":"wait","timeout_ms":10000}}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"parent synthesis after edge child review","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))]),ProviderScript::new("delegated child actual execution", |request| request.path=="/v1/chat/completions" && request.body["model"]=="test-model" && delegation_assessment(&request.body).is_none() && !primary_request_for(request,"Use a child agent to review the edge workspace."),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                            tool_call(
                                "call-child-read-file",
                                "read_file",
                                json!({"path": "/workspace/astra/src/lib.rs"})
                            )
                        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"edge child reviewed concrete file evidence: pub fn run()","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))]),ProviderScript::new("canonical delegation assessment", |request| request.path=="/v1/chat/completions" && delegation_assessment(&request.body).is_some(),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"{\"disposition\":\"not_applicable\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":32,"completion_tokens":8,"total_tokens":40}}))])]).await;

    let response = chat_stream_start(
        &app,
        json!({
            "message": "Use a child agent to review the edge workspace.",
            "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
            "workspace_binding": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "root": "/workspace/astra",
                "source": {
                    "kind": "edge_path",
                    "path": "/workspace/astra"
                },
                "authority": "read_write"},
            "executor_binding": {
                "kind": "edge_agent",
                "executor_id": DEFAULT_TEST_EDGE_AGENT_ID,
                "display_name": "MacBook Pro",
                "transport": "edge_ledger",
                "status": "online"
            },
            "context": {

            }
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (mut rx, reader) = spawn_sse_reader(response.into_body()).await;
    // This includes two parent rounds plus child admission before the first
    // edge request. Keep it bounded, but allow the same 10s budget as the
    // child completion below when CI is running another E2E concurrently.
    let child_request = wait_for_sse(&mut rx, "tool_request", 10).await;
    assert_eq!(child_request["tool"], "read_file");
    assert_eq!(child_request["request_id"], "call-child-read-file");
    assert_eq!(
        post_tool_result_from_event(
            &app,
            &child_request,
            "pub fn run() -> Result<()> { Ok(()) }",
            "completed",
        )
        .await,
        StatusCode::OK
    );
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("edge child stream timed out")
        .expect("edge child stream reader failed");
    assert_native_delegation_judgment(
        &gateway,
        "Use a child agent to review the edge workspace.",
        &[(
            "edge child review",
            "Review src/lib.rs in the inherited edge workspace.",
        )],
    )
    .await;

    let serialized = serde_json::to_string(&events).unwrap();
    assert!(
        find_events(&events, "text_delta").iter().any(|event| {
            event["content"].as_str() == Some("parent synthesis after edge child review")
        }),
        "parent should synthesize after edge-bound child spawn: {serialized}"
    );
    assert!(
        serialized.contains("edge child reviewed concrete file evidence: pub fn run()"),
        "the child must synthesize after receiving real workspace evidence: {serialized}"
    );
    let spawn_result = find_events(&events, "tool_call_end")
        .into_iter()
        .find(|event| event["call_id"].as_str() == Some("call-spawn-edge-reviewer"))
        .and_then(|event| event["result"].as_str())
        .and_then(|result| serde_json::from_str::<Value>(result).ok())
        .unwrap_or_else(|| panic!("edge spawn must return a canonical result: {serialized}"));
    assert_eq!(spawn_result["status"], "launched", "{serialized}");
    assert_eq!(spawn_result["result_family"], "control_receipt");
    assert_eq!(spawn_result["success"], true);
    assert_child_joined_before_parent(
        &events,
        &spawn_result["agent_id"],
        "edge child reviewed concrete file evidence: pub fn run()",
        "parent synthesis after edge child review",
    );

    let workspace = find_event(&events, "workspace_bound")
        .unwrap_or_else(|| panic!("expected workspace_bound event: {serialized}"));
    assert_eq!(workspace["workspace"]["kind"], "edge_workspace");
    assert_eq!(workspace["workspace"]["cwd"], "/workspace/astra");
    assert_eq!(workspace["executor"]["kind"], "edge_agent");
    assert_eq!(workspace["transport"], "edge_ledger");

    let routing = find_events(&events, "tool_routing_decision")
        .into_iter()
        .find(|event| {
            event["call_id"].as_str() == Some("call-spawn-edge-reviewer")
                && event["tool"].as_str() == Some("agent")
        })
        .unwrap_or_else(|| {
            panic!("expected server-control-plane route for edge-bound agent spawn: {serialized}")
        });
    assert_eq!(routing["route"], "server_control_plane");

    let live_events = find_events(&events, "agent_live_event");
    let live_output = live_events
        .iter()
        .find(|event| {
            event["event_kind"].as_str() == Some("output_delta")
                && event["content"].as_str()
                    == Some("edge child reviewed concrete file evidence: pub fn run()")
        })
        .unwrap_or_else(|| {
            panic!("edge-bound dynamic spawn should stream child output into agent_live_event: {serialized}")
        });
    assert_eq!(live_output["workspace"]["kind"], "edge_workspace");
    assert_eq!(live_output["workspace"]["cwd"], "/workspace/astra");
    assert_eq!(live_output["executor"]["kind"], "edge_agent");
    assert_eq!(
        live_output["executor"]["executor_id"],
        DEFAULT_TEST_EDGE_AGENT_ID
    );
    assert_eq!(live_output["transport"], "edge_ledger");

    let spawned = find_event_type(&events, "agent_spawned");
    assert!(
        !spawned.is_empty(),
        "edge-bound dynamic spawn should emit agent_spawned progress: {serialized}"
    );
    assert_eq!(spawned[0]["workspace"]["kind"], "edge_workspace");
    assert_eq!(spawned[0]["workspace"]["cwd"], "/workspace/astra");
    assert_eq!(spawned[0]["executor"]["kind"], "edge_agent");
    assert_eq!(
        spawned[0]["executor"]["executor_id"],
        DEFAULT_TEST_EDGE_AGENT_ID
    );
    assert_eq!(spawned[0]["transport"], "edge_ledger");
    let child_run_id = spawned[0]["run_id"].as_str().expect("agent_spawned run_id");
    assert_eq!(
        find_events(&events, "tool_call_end")
            .into_iter()
            .filter(|event| event["call_id"].as_str() == Some("call-child-read-file"))
            .count(),
        0,
        "the client-owned callback lane must not be duplicated onto the attached parent stream"
    );
    let (child_replay_status, child_replay) = get_run_stream(&app, child_run_id, 0).await;
    assert_eq!(child_replay_status, StatusCode::OK);
    let child_terminal_index = child_replay
        .iter()
        .position(|event| {
            event["type"].as_str() == Some("tool_call_end")
                && event["call_id"].as_str() == Some("call-child-read-file")
        })
        .unwrap_or_else(|| panic!("child replay lost its durable tool terminal: {child_replay:?}"));
    let child_finished_index = child_replay
        .iter()
        .position(|event| event["type"].as_str() == Some("run_finished"))
        .unwrap_or_else(|| panic!("child replay lost run_finished: {child_replay:?}"));
    assert!(
        child_terminal_index < child_finished_index,
        "child tool terminal must commit before run_finished: {child_replay:?}"
    );
    assert_eq!(
        child_replay
            .iter()
            .filter(|event| {
                event["type"].as_str() == Some("tool_call_end")
                    && event["call_id"].as_str() == Some("call-child-read-file")
            })
            .count(),
        1,
        "child replay must expose exactly one durable terminal occurrence"
    );

    let completed = find_event_type(&events, "agent_completed");
    assert!(
        !completed.is_empty(),
        "edge-bound dynamic spawn should emit agent_completed progress: {serialized}"
    );
    assert_eq!(completed[0]["workspace"]["kind"], "edge_workspace");
    assert_eq!(completed[0]["executor"]["kind"], "edge_agent");
    assert_eq!(completed[0]["transport"], "edge_ledger");

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(gateway.requests.lock().await.len(), 7);
    assert_eq!(inference.attempt_count(), 7);
}

#[tokio::test]
async fn discovery_only_child_keeps_native_tool_and_first_request_budget() {
    const ROOT: &str = "Delegate one discovery-only GitHub availability check.";
    const CHILD: &str =
        "Select github once and return CHILD_GITHUB_UNAVAILABLE when it is missing.";
    const MARKER: &str = "CHILD_GITHUB_UNAVAILABLE";
    let response = |calls: Vec<Value>, text: &str| {
        ProviderResponse::OpenAi(json!({
            "choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":calls},
                "finish_reason":if calls.is_empty(){"stop"}else{"tool_calls"}}],
            "usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}
        }))
    };
    let (app, gateway, inference) = build_native_test_app(vec![
        ProviderScript::new("discovery parent", |request| primary_request_for(request, ROOT) && delegation_assessment(&request.body).is_none(), vec![
            response(vec![tool_call("select-child-agent", "tool_search", json!({"query":"select:agent"}))], ""),
            response(vec![tool_call("spawn-discovery-child", "invoke_tool", json!({"name":"agent","arguments":{
                "action":"spawn","description":"discover GitHub availability","prompt":CHILD,
                "agent_type":"explore","allowed_tools":["tool_search"],"max_output_tokens":256
            }}))], ""),
            response(vec![tool_call("join-discovery-child", "agent", json!({"action":"wait","timeout_ms":10000}))], ""),
            response(vec![], MARKER),
        ]),
        ProviderScript::new("discovery child", |request| primary_request_for(request, CHILD) && delegation_assessment(&request.body).is_none(), vec![
            response(vec![tool_call("child-select-github", "tool_search", json!({"query":"select:github"}))], ""),
            response(vec![], MARKER),
        ]),
        ProviderScript::new("delegation assessment", |request| delegation_assessment(&request.body).is_some(), vec![
            response(vec![], "{\"disposition\":\"not_applicable\"}"),
        ]),
    ]).await;
    let response = chat_stream_start(&app, json!({
        "message":ROOT,
        "allow_skills":[],
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "workspace_binding":{"kind":"edge_workspace","display_name":"test workspace","root":"/workspace/astra",
            "source":{"kind":"edge_path","path":"/workspace/astra"},"authority":"read_write"},
        "executor_binding":{"kind":"edge_agent","executor_id":DEFAULT_TEST_EDGE_AGENT_ID,
            "display_name":"test executor","transport":"edge_ledger","status":"online"}
    })).await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = read_sse_events_from_body(response.into_body()).await;
    assert!(
        find_events(&events, "tool_request").is_empty(),
        "discovery must execute on Server without a client callback"
    );
    let spawn = find_events(&events, "tool_call_end")
        .into_iter()
        .find(|event| event["call_id"] == "spawn-discovery-child")
        .and_then(|event| event["result"].as_str())
        .map(|text| serde_json::from_str::<Value>(text).unwrap())
        .expect("launch receipt");
    assert_eq!(spawn["status"], "launched");
    assert_child_joined_before_parent(&events, &spawn["agent_id"], MARKER, MARKER);
    let (status, replay) = get_run_stream(&app, spawn["run_id"].as_str().unwrap(), 0).await;
    assert_eq!(status, StatusCode::OK);
    let finished = find_event(&replay, "run_finished").expect("durable child terminal");
    assert_eq!(finished["status"], "completed");
    assert_eq!(
        finished["turn_evaluation"]["metadata"]["tool_execution_count"],
        1
    );
    assert_eq!(
        finished["turn_evaluation"]["metadata"]["tool_rejected_count"],
        0
    );
    assert_eq!(
        find_event(&replay, "text_done").unwrap()["full_text"],
        MARKER
    );
    let requests = gateway.requests.lock().await;
    let root_first = requests
        .iter()
        .find(|request| primary_request_for(request, ROOT))
        .unwrap();
    let agent = root_first.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "agent")
        .unwrap();
    assert_eq!(
        agent["function"]["parameters"]["properties"]["max_output_tokens"]["minimum"],
        1
    );
    let child: Vec<_> = requests
        .iter()
        .filter(|request| {
            primary_request_for(request, CHILD) && delegation_assessment(&request.body).is_none()
        })
        .collect();
    assert_eq!(child.len(), 2);
    for request in &child {
        let mut names: Vec<_> = request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["invoke_tool", "tool_search"]);
    }
    assert_eq!(child[0].body["max_completion_tokens"], 256);
    assert!(
        child[1].body["max_completion_tokens"].as_u64().unwrap() > 256,
        "only the first child request is capped"
    );
    let results: Vec<Value> = child[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["role"] == "tool" && message["tool_call_id"] == "child-select-github"
        })
        .map(|message| serde_json::from_str(message["content"].as_str().unwrap()).unwrap())
        .collect();
    let paired_calls = child[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .filter_map(|message| message["tool_calls"].as_array())
        .flatten()
        .filter(|call| call["id"] == "child-select-github")
        .count();
    assert_eq!(paired_calls, 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["missing"], json!(["github"]));
    assert_eq!(requests.len(), 7);
    drop(requests);
    assert_eq!(inference.attempt_count(), 7);
    inference.assert_quiescent();
    gateway.assert_complete();
}

// ── Event-driven synchronization helpers ─────────────────────────────────────

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

// Model, tool, and status setup can legitimately exceed a cold CI scheduler slice.
// Keep integration waits bounded while allowing the run to establish.
const E2E_WAIT_TIMEOUT_SECS: u64 = 30;

/// Spawn a background task that reads SSE events from a streaming body,
/// sending each event through an unbounded channel for real-time consumption.
/// Returns (receiver, join_handle). The join handle resolves to all collected events.
async fn spawn_sse_reader(body: Body) -> (mpsc::UnboundedReceiver<Value>, JoinHandle<Vec<Value>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut events = Vec::new();
        let mut buf = String::new();
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            buf.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(idx) = buf.find("\n\n") {
                let event_str = buf[..idx].to_string();
                buf = buf[idx + 2..].to_string();
                if let Some(data) = event_str.strip_prefix("data: ")
                    && let Ok(v) = serde_json::from_str::<Value>(data)
                {
                    let _ = tx.send(v.clone());
                    events.push(v);
                }
            }
        }
        events
    });
    (rx, handle)
}

/// Wait for an SSE event of a specific type from the channel (with timeout).
async fn wait_for_sse(
    rx: &mut mpsc::UnboundedReceiver<Value>,
    event_type: &str,
    timeout_secs: u64,
) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(Value::as_str) == Some(event_type) {
                    return event;
                }
                seen.push(event);
            }
            Ok(None) => {
                let summary = seen
                    .iter()
                    .map(|event| {
                        json!({
                            "type": event.get("type"),
                            "call_id": event.get("call_id"),
                            "request_id": event.get("request_id"),
                            "event_kind": event.get("event_kind"),
                            "status": event.get("status"),
                            "content": event.get("content"),
                            "result": event.get("result"),
                            "error": event.get("error"),
                            "error_code": event.get("error_code"),
                            "details": event.get("details"),
                            "tool": event.get("tool"),
                            "tool_call": event.get("tool_call"),
                        })
                    })
                    .collect::<Vec<_>>();
                panic!("stream ended without '{event_type}' event; seen={summary:#?}")
            }
            Err(_) => {
                panic!(
                    "timed out ({timeout_secs}s) waiting for '{event_type}' event; seen={seen:#?}"
                )
            }
        }
    }
}

#[derive(Clone)]
struct EdgeCallbackStep {
    request_id: &'static str,
    tool_name: &'static str,
    args: Value,
    result_output: &'static str,
    requires_approval: bool,
}

#[derive(Clone)]
struct EdgeCallbackScenario {
    name: &'static str,
    message: String,
    edge_tools: Vec<&'static str>,
    steps: Vec<EdgeCallbackStep>,
    final_text: &'static str,
}

async fn execute_edge_callback_turn(
    app: &Router,
    payload: Value,
    case_name: &str,
    steps: &[EdgeCallbackStep],
    final_text: &str,
) -> Vec<Value> {
    let resp = chat_stream_start(app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;
    let approval_identity = wait_for_approval_identity(&mut rx).await;

    for step in steps {
        if !test_tool_uses_client_ledger(step.tool_name) {
            continue;
        }
        if step.requires_approval {
            let approval = wait_for_sse(&mut rx, "approval_required", E2E_WAIT_TIMEOUT_SECS).await;
            assert_eq!(
                approval["request_id"].as_str(),
                Some(step.request_id),
                "{}: approval should match {}",
                case_name,
                step.request_id
            );
            let status =
                post_approval_respond(app, &approval_identity, step.request_id, "allow").await;
            assert_eq!(status, StatusCode::OK, "{}: approval accepted", case_name);
        }

        let request = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
        assert_eq!(
            request["request_id"].as_str(),
            Some(step.request_id),
            "{}: tool_request should match {}",
            case_name,
            step.request_id
        );
        let status =
            post_tool_result_from_event(app, &request, step.result_output, "completed").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}: tool result accepted",
            case_name
        );
    }

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .unwrap_or_else(|_| panic!("{case_name}: stream timed out"))
        .expect("reader task failed");
    assert!(
        find_events(&events, "text_delta")
            .iter()
            .any(|event| event["content"].as_str() == Some(final_text)),
        "{}: expected final text",
        case_name
    );
    events
}

async fn run_tool_scenario(case: EdgeCallbackScenario) {
    assert!(!case.steps.is_empty());
    assert!(
        case.steps
            .iter()
            .all(|step| test_tool_uses_client_ledger(step.tool_name))
    );
    let message = case.message.clone();
    let tool_calls: Vec<Value> = case
        .steps
        .iter()
        .map(|step| tool_call(step.request_id, step.tool_name, step.args.clone()))
        .collect();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new(case.name, move |request| primary_request_for(request, &message), vec![
        ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","tool_calls":tool_calls},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
        ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":case.final_text},"finish_reason":"stop"}],"usage":{"prompt_tokens":52,"completion_tokens":8,"total_tokens":60}})),
    ])]).await;
    let edge_tools: Vec<Value> = case
        .edge_tools
        .iter()
        .map(|tool| tool_schema(tool))
        .collect();
    let events = execute_edge_callback_turn(
        &app,
        json!({
            "message": &case.message,
            "interactive_client": case.steps.iter().any(|step| step.requires_approval),
            "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
            "context": {"edge_tools": edge_tools}
        }),
        case.name,
        &case.steps,
        case.final_text,
    )
    .await;
    assert_eq!(find_events(&events, "turn_complete").len(), 1);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let followup = requests[1].body["messages"].as_array().unwrap();
    for step in &case.steps {
        assert!(
            followup.iter().any(|message| message["role"] == "tool"
                && message["tool_call_id"] == step.request_id
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(step.result_output))),
            "{}: actual callback must reach the next provider request",
            case.name
        );
        assert_eq!(
            find_events(&events, "tool_request")
                .iter()
                .filter(|event| event["request_id"] == step.request_id)
                .count(),
            1
        );
    }
}

/// Poll run status until it reaches the expected value (with timeout).
async fn poll_run_status(app: &Router, run_id: &str, expected: &str, timeout_secs: u64) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let (st, body) = get_run_status(app, run_id).await;
        if st == StatusCode::OK && body["status"].as_str().unwrap_or("") == expected {
            return body;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out ({timeout_secs}s) waiting for run '{run_id}' → '{expected}'");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Poll an async condition with timeout. Returns when the predicate returns true.
async fn poll_until<F, Fut>(predicate: F, timeout_secs: u64)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if predicate().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("poll_until timed out after {timeout_secs}s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// TESTS
// ══════════════════════════════════════════════════════════════════════════════

// ── Basic streaming: text-only response ──────────────────────────────────────

#[tokio::test]
async fn text_only_response_streams_session_info_and_text() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("text_only_response_streams_session_info_and_text", |request| primary_request_for(request, "Hello"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Hi there!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let payload = json!({
        "message": "Hello",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let events = chat_stream_collect(&app, payload).await;

    // First event should be session_info.
    assert!(
        events.len() >= 2,
        "expected session_info + text events, got {}",
        events.len()
    );
    let session_info = &events[0];
    assert_eq!(session_info["type"], "session_info");
    let session_id = session_info["session_id"].as_str().unwrap();
    assert!(!session_id.is_empty());
    assert!(session_id.starts_with("web-e2e-"));
    uuid::Uuid::parse_str(session_id.strip_prefix("web-e2e-").unwrap()).unwrap();
    uuid::Uuid::parse_str(session_info["run_id"].as_str().unwrap()).unwrap();

    // Should have text_delta event.
    let text_events = find_events(&events, "text_delta");
    assert!(
        !text_events.is_empty(),
        "expected at least one text_delta event"
    );
    assert_eq!(text_events.len(), 1, "one plain answer is projected once");
    assert_eq!(text_events[0]["content"], "Hi there!");
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn web_agent_stream_emits_workspace_and_executor_binding_snapshots() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("web_agent_stream_emits_workspace_and_executor_binding_snapshots", |request| primary_request_for(request, "Hello"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Hi there!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "Hello",
            "workspace_binding": {
                "kind": "server_sandbox",
                "display_name": "Server sandbox",
                "authority": "read_write",
            },
            "executor_binding": {
                "kind": "server_local",
                "executor_id": "server-local",
                "display_name": "Server sandbox",
                "transport": "server_local",
                "status": "online"
            },
            "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
        }),
    )
    .await;

    let run_id = find_event(&events, "session_info")
        .and_then(|event| event["run_id"].as_str())
        .expect("session_info run_id");
    let workspace = find_event(&events, "workspace_bound")
        .unwrap_or_else(|| panic!("expected workspace_bound event: {events:?}"));
    assert_eq!(workspace["run_id"], run_id);
    assert_eq!(workspace["workspace"]["kind"], "server_sandbox");
    assert_eq!(workspace["executor"]["kind"], "server_local");
    assert_eq!(workspace["transport"], "server_local");
    assert!(
        workspace["workspace"]["cwd"].as_str().is_some_and(|cwd| {
            cwd.contains("astra-workspaces") && !cwd.contains("client/claimed")
        }),
        "workspace cwd should be the provisioned server workspace: {workspace:?}"
    );

    let executor = find_event(&events, "executor_bound")
        .unwrap_or_else(|| panic!("expected executor_bound event: {events:?}"));
    assert_eq!(executor["run_id"], run_id);
    assert_eq!(executor["executor"]["status"], "online");
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn web_agent_tool_call_events_include_execution_binding_metadata() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("web_agent_tool_call_events_include_execution_binding_metadata", |request| primary_request_for(request,"Run a command in the workspace"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                            tool_call("call-bash-binding", "bash", json!({"command": "printf ok"}))
                        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Command finished.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let response = chat_stream_start(
        &app,
        json!({
            "message": "Run a command in the workspace",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "workspace_binding": {
                "kind": "server_sandbox",
                "display_name": "Server sandbox",
                "authority": "read_write"},
            "executor_binding": {
                "kind": "server_local",
                "executor_id": "server-local",
                "display_name": "Server sandbox",
                "transport": "server_local",
                "status": "online"
            },
            "context": {

            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(response.into_body()).await;
    let approval_identity = wait_for_approval_identity(&mut rx).await;
    let approval = wait_for_sse(&mut rx, "approval_required", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(approval["tool"].as_str(), Some("bash"));
    let approval_request_id = approval["request_id"]
        .as_str()
        .expect("canonical approval request id");
    let (status, response_body) =
        post_approval_respond_with_body(&app, &approval_identity, approval_request_id, "allow")
            .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "server-owned bash approval: {response_body}"
    );
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out after approval")
        .expect("reader task failed");

    let tool_call = find_event(&events, "tool_call")
        .unwrap_or_else(|| panic!("expected tool_call event: {events:?}"));
    assert_eq!(tool_call["tool_call"]["id"], "call-bash-binding");
    // The provider event is an intent, not an execution receipt.  It must not
    // claim the host's default owner before admission selects a route.
    assert!(tool_call.get("workspace").is_none());
    assert!(tool_call.get("executor").is_none());
    assert!(tool_call.get("transport").is_none());

    // Route-owned lifecycle events are the authoritative execution evidence.
    let tool_end = find_events(&events, "tool_call_end")
        .into_iter()
        .find(|event| event["call_id"] == "call-bash-binding")
        .unwrap_or_else(|| panic!("expected routed tool_call_end event: {events:?}"));
    assert_eq!(tool_end["workspace"]["kind"], "server_sandbox");
    assert_eq!(tool_end["executor"]["kind"], "server_local");
    assert_eq!(tool_end["transport"], "server_local");
    assert!(
        tool_end["workspace"]["cwd"].as_str().is_some_and(|cwd| {
            cwd.contains("astra-workspaces") && !cwd.contains("client/claimed")
        }),
        "tool_call_end should carry the actual provisioned workspace: {tool_end:?}"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn offline_edge_capabilities_are_not_exposed_or_dispatched() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("offline_edge_capabilities_are_not_exposed_or_dispatched", |request| primary_request_for(request,"Run a command in my edge workspace"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                            tool_call(
                                "call-edge-offline-bash",
                                "bash",
                                json!({"command": "printf should-not-run"})
                            )
                        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"The selected edge executor is offline; reconnect it before running the command."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "Run a command in my edge workspace",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "workspace_binding": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "root": "/workspace/astra",
                "source": {
                    "kind": "edge_path",
                    "path": "/workspace/astra"
                },
                "authority": "read_write"},
            "executor_binding": {
                "kind": "edge_agent",
                "executor_id": "edge-macbook-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "offline"
            },
            "context": {"edge_tools":[tool_schema("bash")]}
        }),
    )
    .await;

    let context = find_event(&events, "context_meta").unwrap();
    let visible = context["visible_tools"].as_array().unwrap();
    assert!(!visible.iter().any(|tool| tool == "bash"));
    let coverage = find_event(&events, "executor_bound").unwrap();
    assert_eq!(coverage["executor"]["status"], "offline");
    assert_eq!(coverage["workspace"]["kind"], "edge_workspace");
    assert!(find_events(&events, "tool_request").is_empty());
    assert!(find_events(&events, "tool_call_end").is_empty());
    let text = find_events(&events, "text_delta")
        .iter()
        .map(|event| event["content"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(
        text,
        "The selected edge executor is offline; reconnect it before running the command."
    );
    assert_eq!(find_events(&events, "turn_complete").len(), 1);

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn text_with_reasoning_streams_both() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("text_with_reasoning_streams_both", |request| primary_request_for(request, "Think step by step"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"The answer is 42.", "reasoning_content": "Let me think about this..."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let payload = json!({
        "message": "Think step by step",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let events = chat_stream_collect(&app, payload).await;

    let reasoning = find_events(&events, "reasoning_delta");
    assert!(!reasoning.is_empty(), "expected reasoning_delta events");
    assert_eq!(reasoning[0]["content"], "Let me think about this...");

    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty());
    assert_eq!(text[0]["content"], "The answer is 42.");
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn usage_event_emitted() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("usage_event_emitted", |request| primary_request_for(request, "hello"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150}}))])]).await;

    let payload = json!({
        "message": "hello",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let events = chat_stream_collect(&app, payload).await;
    let usage = find_events(&events, "usage");
    assert!(!usage.is_empty(), "expected usage event");
    assert_eq!(usage[0]["input_tokens"], 100);
    assert_eq!(usage[0]["output_tokens"], 50);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn edge_batch_and_sequential_callbacks_preserve_usage_history_and_cleanup() {
    const MESSAGE: &str = "Read and list the batch, search it, then find source files.";
    fn response(
        text: &str,
        reasoning: &str,
        calls: Vec<Value>,
        input: u64,
        output: u64,
    ) -> ProviderResponse {
        let finish = if calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        ProviderResponse::OpenAi(
            json!({"choices":[{"index":0,"message":{"role":"assistant","content":text,"reasoning_content":reasoning,"tool_calls":calls},"finish_reason":finish}],"usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output}}),
        )
    }
    let complex_args = json!({"path":"/file0", "options":{"encoding":"utf-8","line_numbers":true,"range":[1,100]},"metadata":{"tags":["rust","source"],"nested":{"deep":{"value":42}}}});
    let batch: Vec<Value> = (0..5)
        .map(|i| {
            tool_call(
                &format!("batch-{i}"),
                if i == 4 { "list_dir" } else { "read_file" },
                if i == 4 {
                    json!({})
                } else if i == 0 {
                    complex_args.clone()
                } else {
                    json!({"path":format!("/file{i}")})
                },
            )
        })
        .collect();
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        "actual batch and three tool rounds",
        |request| primary_request_for(request, MESSAGE),
        vec![
            response(
                "Checking the batch.",
                "I need the file evidence.",
                batch,
                42,
                7,
            ),
            response(
                "",
                "",
                vec![tool_call(
                    "search-next",
                    "grep",
                    json!({"pattern":"TODO","path":"."}),
                )],
                52,
                8,
            ),
            response(
                "",
                "",
                vec![tool_call("glob-last", "glob", json!({"pattern":"*.rs"}))],
                62,
                9,
            ),
            response("Completed the file analysis.", "", Vec::new(), 72, 10),
        ],
    )])
    .await;
    let inference = InferenceLedgerFixture::default();
    let (app, hook, observer, ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );
    let mut read_schema = tool_schema("read_file");
    read_schema["function"]["parameters"]["properties"]["options"] = json!({"type":"object"});
    read_schema["function"]["parameters"]["properties"]["metadata"] = json!({"type":"object"});
    let response = chat_stream_start(&app,json!({"message":MESSAGE,"execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},"context":{"edge_tools":[read_schema,tool_schema("grep"),tool_schema("list_dir"),tool_schema("glob")]}})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (mut rx, reader) = spawn_sse_reader(response.into_body()).await;
    let large = "y".repeat(50_000);
    let outputs: Vec<(String, String)> = (0..5)
        .map(|i| {
            (
                format!("batch-{i}"),
                if i == 3 {
                    large.clone()
                } else {
                    format!("content of file{i}")
                },
            )
        })
        .chain([
            ("search-next".into(), "TODO matches: 3".into()),
            ("glob-last".into(), "main.rs\nlib.rs\nmod.rs".into()),
        ])
        .collect();
    for (index, (id, output)) in outputs.iter().enumerate() {
        let request = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
        assert_eq!(request["request_id"], *id);
        if index == 0 {
            assert_eq!(request["args"], complex_args);
            assert_eq!(
                gateway.requests.lock().await.len(),
                1,
                "Server must wait for callbacks in this execution"
            );
            let run_id = request["run_id"].as_str().unwrap();
            let (status, body) = get_run_status(&app, run_id).await;
            assert_eq!(status, StatusCode::OK);
            assert_ne!(body["status"], "completed");
        }
        assert_eq!(
            post_tool_result_from_event(&app, &request, output, "completed").await,
            StatusCode::OK
        );
    }
    let events = tokio::time::timeout(std::time::Duration::from_secs(15), reader)
        .await
        .expect("stream deadline")
        .unwrap();
    let session = find_event(&events, "session_info").unwrap();
    let body = poll_run_status(
        &app,
        session["run_id"].as_str().unwrap(),
        "completed",
        E2E_WAIT_TIMEOUT_SECS,
    )
    .await;
    assert_eq!(body["run_id"], session["run_id"]);
    assert_eq!(body["session_id"], session["session_id"]);
    assert!(body["waiting_for"].is_null());
    assert!(body["events_count"].as_u64().unwrap() > 0);
    assert_eq!(find_events(&events, "tool_call").len(), 7);
    assert_eq!(find_events(&events, "tool_request").len(), 7);
    assert_eq!(find_events(&events, "turn_complete").len(), 1);
    let text = find_events(&events, "text_delta")
        .into_iter()
        .map(|event| event["content"].as_str().unwrap())
        .collect::<String>();
    assert!(text.contains("Checking the batch."));
    assert!(text.contains("Completed the file analysis."));
    assert!(!find_events(&events, "reasoning_delta").is_empty());
    let usage = find_events(&events, "usage");
    assert!(usage.len() >= 4);
    assert_eq!(usage.last().unwrap()["input_tokens"], 228);
    assert_eq!(usage.last().unwrap()["output_tokens"], 34);
    poll_until(
        || {
            let observer = observer.clone();
            async move { !observer.requests.lock().await.is_empty() }
        },
        5,
    )
    .await;
    let observer_requests = observer.requests.lock().await;
    assert_eq!(observer_requests.len(), 1);
    let messages = &observer_requests[0].messages;
    let first_batch = messages
        .iter()
        .find_map(|message| message.get("tool_calls").and_then(Value::as_array))
        .unwrap();
    assert_eq!(
        first_batch
            .iter()
            .map(|call| call["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        outputs[..5]
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        serde_json::from_str::<Value>(first_batch[0]["function"]["arguments"].as_str().unwrap())
            .unwrap(),
        complex_args
    );
    let result_ids = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|message| message.get("tool_call_id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        result_ids,
        outputs
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
    );
    assert!(messages.iter().all(|message| {
        message.get("role").and_then(Value::as_str) != Some("assistant")
            || message.get("content") != Some(&Value::Null)
            || message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
    }));
    assert!(hook.plans.lock().await.is_empty());
    poll_until(
        || {
            let ledger = ledger.clone();
            async move { ledger.lock().await.is_empty() }
        },
        5,
    )
    .await;
    assert!(ledger.lock().await.is_empty());
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 4);
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 4);
    for (id, output) in &outputs {
        let content = requests.last().unwrap().body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == *id)
            .and_then(|message| message["content"].as_str())
            .expect("every actual callback retains its exact tool identity");
        if id == "batch-3" {
            assert!(content.starts_with("<persisted-output>"));
            assert!(content.contains("50000 chars"));
            assert!(content.contains("Tool result id: batch-3"));
            assert!(content.contains(&"y".repeat(128)));
            assert!(content.len() < output.len());
        } else {
            assert!(content.contains(output));
        }
    }
}

#[tokio::test]
async fn skill_tool_call_is_intercepted_without_edge_tool_request() {
    init_env();
    let gateway=ProviderGateway::start(vec![ProviderScript::new("skill_tool_call_is_intercepted_without_edge_tool_request", |request| request.path == "/v1/chat/completions" && request.body["model"] == "MiniMax-M2.7" && request.body["stream"] == true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message| message["role"] == "user" && message["content"] == "Use the test skill")), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                        tool_call("tc-skill-1", "skill", json!({"skill_name": "test-skill"}))
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"I used the skill instructions.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, hook_writer, observer_worker) = build_test_app_with_hooks_and_skills(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let payload = json!({
        "message": "Use the test skill",
    "model_selection":{"offering_id":"model-MiniMax-M2.7"},
    "context":{"edge_profile":{"active_skills":["concise","markdown"],"cwd":"/workspace/astra","git_branch":"main"}},
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"}
    });

    let events = chat_stream_collect(&app, payload).await;

    assert!(
        find_events(&events, "tool_request").is_empty(),
        "intercepted skill should not fall through to edge tool execution"
    );

    let ow = observer_worker.clone();
    poll_until(
        || {
            let ow = ow.clone();
            async move { !ow.requests.lock().await.is_empty() }
        },
        5,
    )
    .await;

    let requests = observer_worker.requests.lock().await;
    assert_eq!(requests.len(), 1, "expected one observer request");
    let result = requests[0]
        .messages
        .iter()
        .find(|message| message.get("tool_call_id").and_then(Value::as_str) == Some("tc-skill-1"))
        .and_then(|message| message.get("content").and_then(Value::as_str))
        .unwrap_or("");
    assert!(
        result.contains("<skill-loaded name=\"test-skill\"/>"),
        "skill result should be injected into the turn: {result}"
    );

    assert!(result.contains("You are the test skill. Return the prepared instructions."));
    assert!(result.find("You are the test skill").unwrap() < result.find("<skill-loaded").unwrap());
    assert!(!find_events(&events, "context_meta").is_empty());
    assert_eq!(find_events(&events, "tool_call").len(), 1);
    let terminals = find_events(&events, "turn_complete");
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0]["llm_rounds"], 2);
    assert_eq!(terminals[0]["tool_calls_count"], 1);
    let wire = gateway.requests.lock().await;
    let delivered = wire[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "tc-skill-1")
        .unwrap();
    assert_eq!(delivered["content"], result);
    drop(wire);
    let text_events = find_events(&events, "text_delta");
    assert!(
        text_events
            .iter()
            .any(|event| event["content"].as_str() == Some("I used the skill instructions.")),
        "final LLM round should continue after skill interception"
    );

    poll_until(
        || {
            let hook_writer = hook_writer.clone();
            async move { !hook_writer.plans.lock().await.is_empty() }
        },
        5,
    )
    .await;
    let plans = hook_writer.plans.lock().await;
    assert_eq!(
        plans.len(),
        1,
        "an intercepted skill must persist exactly one selection"
    );
    let skill = plans
        .first()
        .and_then(|plan| plan.skill_selection.as_ref())
        .expect("an intercepted skill must persist its explicit selection");
    assert_eq!(skill.skill_name, "test-skill");
    assert_eq!(skill.selected_skills, vec!["test-skill".to_string()]);
    assert_eq!(skill.selection_method, "llm_skill_choice");

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn cli_thin_client_single_admission_completes_server_owned_multi_round_loop() {
    init_env();
    let gateway=ProviderGateway::start(vec![ProviderScript::new("cli_thin_client_single_admission_completes_server_owned_multi_round_loop", |request| primary_request_for(request,"Use the test skill and report the result"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                        tool_call(
                            "tc-cli-server-skill",
                            "skill",
                            json!({"skill_name": "test-skill"})
                        )
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"The server completed the skill round.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, observer_worker) = build_test_app_with_hooks_and_skills(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind CLI + Server journey listener");
    let address = listener.local_addr().expect("journey listener address");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("serve CLI + Server journey");
    });

    let payload = normalize_chat_stream_payload(json!({
        "message": "Use the test skill and report the result",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "context": {

        }
    }));
    let client = astra_thin_client::ThinClient::new(&format!("http://{address}"), None)
        .expect("construct CLI thin client");
    let response = client
        .post_developer_loop(
            TOKEN.strip_prefix("Bearer ").expect("test bearer token"),
            &payload,
        )
        .await
        .expect("single developer-loop admission");
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(std::time::Duration::from_secs(10), response.text())
        .await
        .expect("CLI + Server stream timed out")
        .expect("read CLI + Server stream");
    let events = parse_sse_events(&body);

    assert_eq!(
        find_events(&events, "session_info").len(),
        1,
        "one admission must establish one canonical run: {events:?}"
    );
    assert!(
        find_events(&events, "tool_request").is_empty(),
        "a server-owned skill must not be delegated back to the CLI: {events:?}"
    );
    assert!(
        find_events(&events, "text_delta").iter().any(|event| {
            event["content"].as_str() == Some("The server completed the skill round.")
        }),
        "the same Server admission must execute the post-tool LLM round: {events:?}"
    );
    let terminals = find_events(&events, "turn_complete");
    assert_eq!(
        terminals.len(),
        1,
        "one user turn has one terminal: {events:?}"
    );
    assert_eq!(terminals[0]["continuation_owner"], "server");
    assert_eq!(terminals[0]["tool_calls_count"], 1);
    assert_eq!(terminals[0]["tools_used"], json!(["skill"]));
    assert_eq!(terminals[0]["llm_rounds"], 2);
    assert!(
        terminals[0]["observation_tool_calls_count"]
            .as_u64()
            .is_some_and(|count| count <= 1),
        "terminal must carry a bounded observation count: {:?}",
        terminals[0]
    );

    let requests = observer_worker.requests.lock().await;
    assert_eq!(
        requests.len(),
        1,
        "one Server loop should emit one observation"
    );
    assert!(requests[0].messages.iter().any(|message| {
        message.get("tool_call_id").and_then(Value::as_str) == Some("tc-cli-server-skill")
    }));
    drop(requests);

    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("CLI + Server test server shutdown timed out")
        .expect("CLI + Server test server join failed");

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn unknown_skill_returns_error_without_edge_tool_request() {
    init_env();
    let gateway=ProviderGateway::start(vec![ProviderScript::new("unknown_skill_returns_error_without_edge_tool_request", |request| primary_request_for(request,"Use a missing skill"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                        tool_call("tc-skill-unknown", "skill", json!({"skill_name": "missing-skill"}))
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"The skill was unavailable.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, observer_worker) = build_test_app_with_hooks_and_skills(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let payload = json!({
        "message": "Use a missing skill",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "context": {

        }
    });

    let events = chat_stream_collect(&app, payload).await;

    assert!(
        find_events(&events, "tool_request").is_empty(),
        "unknown skill should fail in interception, not as an edge tool"
    );

    let ow = observer_worker.clone();
    poll_until(
        || {
            let ow = ow.clone();
            async move { !ow.requests.lock().await.is_empty() }
        },
        5,
    )
    .await;

    let requests = observer_worker.requests.lock().await;
    assert_eq!(requests.len(), 1, "expected one observer request");
    let result = requests[0]
        .messages
        .iter()
        .find(|message| {
            message.get("tool_call_id").and_then(Value::as_str) == Some("tc-skill-unknown")
        })
        .and_then(|message| message.get("content").and_then(Value::as_str))
        .unwrap_or("");
    assert!(
        result.contains("Unknown skill") || result.contains("unknown skill"),
        "unknown skill should surface a clear error: {result}"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

// ── Event ordering ───────────────────────────────────────────────────────────

#[tokio::test]
async fn events_arrive_in_correct_order() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("events_arrive_in_correct_order", |request| primary_request_for(request, "Ordered test"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"The answer.", "reasoning_content": "Thinking..."},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":10,"total_tokens":30}}))])]).await;

    let payload = json!({
        "message": "Ordered test",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let events = chat_stream_collect(&app, payload).await;

    // Expected order: session_info, reasoning_delta, reasoning_done, text_delta, usage
    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str))
        .collect();

    assert_eq!(types[0], "session_info");

    // Reasoning should come before text.
    let reasoning_idx = types
        .iter()
        .position(|&t| t == "reasoning_delta")
        .expect("actual reasoning");
    let text_idx = types
        .iter()
        .position(|&t| t == "text_delta")
        .expect("actual text");
    assert!(reasoning_idx < text_idx, "reasoning must precede text");

    // Usage should be present.
    assert!(types.contains(&"usage"));
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

// ── Concurrent streams don't interfere ───────────────────────────────────────

#[tokio::test]
async fn concurrent_streams_isolated() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![
        ProviderScript::new("stream 1", |request| primary_request_for(request, "Stream 1"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Response for stream 1"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))]),
        ProviderScript::new("stream 2", |request| primary_request_for(request, "Stream 2"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Response for stream 2"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))]),
    ]).await;

    let payload1 = json!({
        "message": "Stream 1",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let payload2 = json!({
        "message": "Stream 2",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let app1 = app.clone();
    let app2 = app.clone();

    let (events1, events2) = tokio::join!(
        chat_stream_collect(&app1, payload1),
        chat_stream_collect(&app2, payload2),
    );

    // Both should have their own session_ids.
    let sid1 = events1[0]["session_id"].as_str().unwrap();
    let sid2 = events2[0]["session_id"].as_str().unwrap();
    assert_ne!(
        sid1, sid2,
        "concurrent streams should have different session IDs"
    );

    // Each should have its own text.
    let text1 = find_events(&events1, "text_delta");
    let text2 = find_events(&events2, "text_delta");
    assert_eq!(text1[0]["content"], "Response for stream 1");
    assert_eq!(text2[0]["content"], "Response for stream 2");
    assert_ne!(events1[0]["run_id"], events2[0]["run_id"]);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

// ── Tool call with error result ──────────────────────────────────────────────

#[tokio::test]
async fn tool_call_with_error_result_continues() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("tool_call_with_error_result_continues", |request| primary_request_for(request,"Try reading"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                        {
                            "id": "tc-err-1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/nonexistent\"}"
                            }
                        }
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Sorry, the file was not found.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let payload = json!({
        "message": "Try reading",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "context": {
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    let request = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(request["request_id"].as_str(), Some("tc-err-1"));
    assert_eq!(
        post_tool_result_from_event(&app, &request, "status=error: file not found", "failed").await,
        StatusCode::OK
    );

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    // Should still get final text.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "LLM should continue after tool error");

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

// ── Approval flow test ──────────────────────────────────────────────────────

#[tokio::test]
async fn tool_requiring_approval_emits_approval_event_and_waits() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("tool_requiring_approval_emits_approval_event_and_waits", |request| primary_request_for(request,"Write a file"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                        {
                            "id": "tc-approve-1",
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": "{\"path\": \"/tmp/out.txt\", \"content\": \"hello\"}"
                            }
                        }
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"File written.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    // write_file requires approval before tool_request is emitted.
    let payload = json!({
        "message": "Write a file",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "interactive_client": true,
        "context": {
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "description": "Write file",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Wait for the approval_required SSE, then approve, then post tool result.
    let approval_identity = wait_for_approval_identity(&mut rx).await;
    wait_for_sse(&mut rx, "approval_required", E2E_WAIT_TIMEOUT_SECS).await;
    let st = post_approval_respond(&app, &approval_identity, "tc-approve-1", "allow").await;
    assert_eq!(st, 200, "approval POST failed");

    let request = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(request["request_id"].as_str(), Some("tc-approve-1"));
    let st = post_tool_result_from_event(&app, &request, "written", "completed").await;
    assert_eq!(st, 200, "tool result POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    // Should have approval_required event.
    let approval_events = find_events(&events, "approval_required");
    assert!(
        !approval_events.is_empty(),
        "expected approval_required event for write_file"
    );

    // Should have tool_request event (after approval granted).
    let tool_requests = find_events(&events, "tool_request");
    assert!(
        !tool_requests.is_empty(),
        "expected tool_request after approval"
    );

    // Should have final text.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "expected final text");

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn approval_batch_does_not_block_earlier_read_only_request() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("approval_batch_does_not_block_earlier_read_only_request", |request| primary_request_for(request,"Read first, then write both files"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                        {
                            "id": "tc-read-first",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/tmp/in.txt\"}"
                            }
                        },
                        {
                            "id": "tc-write-a",
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": "{\"path\": \"/tmp/a.txt\", \"content\": \"A\"}"
                            }
                        },
                        {
                            "id": "tc-write-b",
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": "{\"path\": \"/tmp/b.txt\", \"content\": \"B\"}"
                            }
                        }
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Done.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let payload = json!({
        "message": "Read first, then write both files",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "interactive_client": true,
        "context": {
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read file",
                        "parameters": { "type": "object", "properties": {} }
                    }
                },
                {
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "description": "Write file",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    let approval_identity = wait_for_approval_identity(&mut rx).await;
    let approval = wait_for_sse(&mut rx, "approval_batch_required", E2E_WAIT_TIMEOUT_SECS).await;
    let approval_ids: Vec<_> = approval["requests"]
        .as_array()
        .expect("approval requests")
        .iter()
        .filter_map(|req| req.get("request_id").and_then(Value::as_str))
        .collect();
    assert_eq!(approval_ids, vec!["tc-write-a", "tc-write-b"]);

    let read_request = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(
        read_request["request_id"].as_str(),
        Some("tc-read-first"),
        "earlier read-only call should execute before later approval-gated block"
    );
    let st = post_tool_result_from_event(&app, &read_request, "read-ok", "completed").await;
    assert_eq!(st, 200, "read-only tool result POST failed");

    let st = post_approval_respond(&app, &approval_identity, "tc-write-a", "allow").await;
    assert_eq!(st, 200, "first approval POST failed");
    let st = post_approval_respond(&app, &approval_identity, "tc-write-b", "allow").await;
    assert_eq!(st, 200, "second approval POST failed");

    let write_request_a = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(write_request_a["request_id"].as_str(), Some("tc-write-a"));
    let st = post_tool_result_from_event(&app, &write_request_a, "write-a-ok", "completed").await;
    assert_eq!(st, 200, "first write result POST failed");

    let write_request_b = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(write_request_b["request_id"].as_str(), Some("tc-write-b"));
    let st = post_tool_result_from_event(&app, &write_request_b, "write-b-ok", "completed").await;
    assert_eq!(st, 200, "second write result POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let read_request_pos = events
        .iter()
        .position(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_request")
                && event.get("request_id").and_then(Value::as_str) == Some("tc-read-first")
        })
        .expect("read tool_request");
    let first_write_request_pos = events
        .iter()
        .position(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_request")
                && event.get("request_id").and_then(Value::as_str) == Some("tc-write-a")
        })
        .expect("first write tool_request");
    assert!(
        read_request_pos < first_write_request_pos,
        "read-only request should be emitted before the later approval-gated block"
    );
    assert!(
        !find_events(&events, "text_delta").is_empty(),
        "expected final text after approval batch completes"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

// ── Approval denied → error result ──────────────────────────────────────────

#[tokio::test]
async fn approval_denied_skips_tool_and_continues() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("approval_denied_skips_tool_and_continues", |request| primary_request_for(request,"Write a file"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                        {
                            "id": "tc-deny-1",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": "{\"command\": \"rm -rf /\"}"
                            }
                        }
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Operation was denied.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let payload = json!({
        "message": "Write a file",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "interactive_client": true,
        "context": {
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "description": "Run command",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Deny the approval.
    let approval_identity = wait_for_approval_identity(&mut rx).await;
    wait_for_sse(&mut rx, "approval_required", E2E_WAIT_TIMEOUT_SECS).await;
    let st = post_approval_respond(&app, &approval_identity, "tc-deny-1", "deny").await;
    assert_eq!(st, 200, "approval deny POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    // Should have approval_required event.
    let approval_events = find_events(&events, "approval_required");
    assert!(
        !approval_events.is_empty(),
        "expected approval_required event"
    );

    // Should NOT have tool_request event (denied before execution).
    let tool_requests = find_events(&events, "tool_request");
    assert!(
        tool_requests.is_empty(),
        "denied tool should not emit tool_request"
    );

    // LLM should still continue with final text.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "expected final text after denial");

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

// ══════════════════════════════════════════════════════════════════════════════
// EDGE CASES: Malformed payloads, missing fields, auth failures
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cancelled_edge_run_stops_inference_and_clears_its_callback_ledger() {
    const MESSAGE: &str = "Read the file before cancellation.";
    let gateway=ProviderGateway::start(vec![ProviderScript::new("cancel while waiting for actual Edge callback",|request|primary_request_for(request,MESSAGE),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","tool_calls":[tool_call("native-cancel","read_file",json!({"path":"/src/main.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, ledger) = build_test_app_with_models(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );
    let response=chat_stream_start(&app,json!({"message":MESSAGE,"execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},"context":{"edge_tools":[tool_schema("read_file")]}})).await;
    let (mut rx, reader) = spawn_sse_reader(response.into_body()).await;
    let request = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(request["request_id"], "native-cancel");
    let run_id = request["run_id"].as_str().unwrap();
    let (status, list) = list_runs(&app, 10).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == run_id && run["status"] == "running")
    );
    assert_eq!(cancel_run(&app, run_id).await, StatusCode::OK);
    assert_eq!(
        post_tool_result_from_event(&app, &request, "cancelled", "completed").await,
        StatusCode::OK
    );
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("cancel must finish the original stream")
        .unwrap();
    let body = poll_run_status(&app, run_id, "cancelled", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(body["status"], "cancelled");
    assert_eq!(body["run_id"], run_id);
    assert!(body["waiting_for"].is_null());
    let finished = find_events(&events, "run_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["status"], "cancelled");
    // Cancellation is committed by the Run owner; its run_finished is the terminal.
    assert!(find_events(&events, "turn_complete").is_empty());
    assert!(
        find_events(&events, "text_delta").is_empty(),
        "no post-cancel answer may be synthesized"
    );
    poll_until(
        || {
            let ledger = ledger.clone();
            async move { ledger.lock().await.is_empty() }
        },
        5,
    )
    .await;
    assert!(ledger.lock().await.is_empty());
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(
        gateway.requests.lock().await.len(),
        1,
        "cancellation forbids another provider round"
    );
}

#[tokio::test]
async fn missing_auth_header_returns_unauthorized() {
    init_env();
    let (app, _) = build_test_app();

    // SSE endpoints return HTTP 200 even for errors (SSE convention).
    // Auth failures are sent as SSE error events.
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("content-type", "application/json")
        .body(Body::from(json!({"message": "hi"}).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let events = read_sse_events_from_body(resp.into_body()).await;
    let errors = find_events(&events, "error");
    assert!(
        !errors.is_empty(),
        "expected an error event for missing auth"
    );
    assert_eq!(errors[0]["code"], "AUTH_ERROR");
}

#[tokio::test]
async fn invalid_auth_token_returns_unauthorized() {
    init_env();
    let (app, _) = build_test_app();

    // SSE endpoints return HTTP 200 even for errors.
    // An invalid token produces an SSE error event.
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", "Bearer invalid-token")
        .header("content-type", "application/json")
        .header("x-astra-e2e-test-secret", SECRET)
        .body(Body::from(json!({"message": "hi"}).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let events = read_sse_events_from_body(resp.into_body()).await;
    let errors = find_events(&events, "error");
    assert!(
        !errors.is_empty(),
        "expected an error event for invalid token"
    );
    assert_eq!(errors[0]["code"], "AUTH_ERROR");
}

#[tokio::test]
async fn empty_message_with_user_intent_still_completes() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("empty_message_with_user_intent_still_completes", |request| primary_request_for(request, "respond to the explicit empty-message test intent"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"You sent an empty message."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let payload = json!({
        "message": "",
        "user_intent": "respond to the explicit empty-message test intent",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let events = chat_stream_collect(&app, payload).await;
    let text = find_events(&events, "text_delta");
    assert_eq!(
        text.iter()
            .map(|event| event["content"].as_str().unwrap())
            .collect::<String>(),
        "You sent an empty message.",
        "stream diagnostics: {:?}",
        events
            .iter()
            .map(|event| (
                &event["type"],
                &event["code"],
                &event["message"],
                &event["detail"]
            ))
            .collect::<Vec<_>>()
    );
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn missing_message_field_returns_sse_error() {
    init_env();
    let (app, _) = build_test_app();

    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-astra-e2e-test-secret", SECRET)
        .body(Body::from(
            json!({
                "model_selection": { "offering_id": DEFAULT_MODEL_OFFERING_ID },
                "context": {}
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let events = read_sse_events_from_body(resp.into_body()).await;
    let errors = find_events(&events, "error");
    assert!(
        errors.iter().any(|event| event["message"]
            .as_str()
            .is_some_and(|message| message.contains("missing field `message`"))),
        "missing message field should return a deserialization SSE error: {events:?}"
    );
}

#[tokio::test]
async fn tool_call_without_provider_identity_is_rejected_before_edge_delivery() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("tool_call_without_provider_identity_is_rejected_before_edge_delivery", |request| primary_request_for(request,"auto-id test"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                        {
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/test\"}"
                            }
                        }
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    // Execution identity belongs to the provider. The runtime must not mint
    // one because doing so makes replay, callback, and durable pairing
    // ambiguous.
    let payload = json!({
        "message": "auto-id test",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "context": {
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (_rx, reader) = spawn_sse_reader(resp.into_body()).await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(8), reader)
        .await
        .expect("stream should fail closed without waiting for a callback")
        .expect("SSE reader should not panic");
    assert!(
        find_events(&events, "tool_request").is_empty(),
        "invalid provider identity must not reach edge delivery: {events:?}"
    );
    assert!(
        find_events(&events, "run_error")
            .iter()
            .any(|event| event["message"]
                .as_str()
                .is_some_and(|message| message.contains("provider tool-call protocol violation"))),
        "the identity contract failure must remain observable: {events:?}"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn tool_result_for_unknown_run_is_rejected_without_side_effects() {
    init_env();
    let (app, ledger) = build_test_app();

    // Authentication alone must not authorize an arbitrary callback identity.
    let st = post_unmatched_tool_result(&app, "nonexistent-id-12345", "output", "completed").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(
        ledger.lock().await.is_empty(),
        "an unknown run must not create an orphan callback entry"
    );
}

#[tokio::test]
async fn approval_for_unknown_request_id_is_rejected_without_side_effects() {
    init_env();
    let (app, _) = build_test_app();

    let identity = ApprovalIdentity {
        session_id: "web-e2e-unknown-approval-session".to_string(),
        run_id: "web-e2e-unknown-approval-run".to_string(),
    };
    let st = post_approval_respond(&app, &identity, "nonexistent-approval-id", "allow").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ══════════════════════════════════════════════════════════════════════════════
// STRESS: Large responses, many tool calls, deep nesting
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn large_text_response_streams_completely() {
    init_env();
    let large_text = "x".repeat(10_000);
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("large_text_response_streams_completely", |request| primary_request_for(request, "Generate a long response"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":large_text.clone()},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    // Generate a large text response (~10KB).
    let payload = json!({
        "message": "Generate a long response",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let events = chat_stream_collect(&app, payload).await;
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty());
    let content: String = text
        .iter()
        .map(|event| event["content"].as_str().unwrap())
        .collect();
    assert_eq!(content, large_text, "every provider chunk must be retained");
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn approval_allow_session_approves_tool() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("approval_allow_session_approves_tool", |request| primary_request_for(request,"Session-wide approval"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[{
                        "id": "tc-session-approve",
                        "type": "function",
                        "function": { "name": "write_file", "arguments": "{\"path\": \"/out\"}" }
                    }]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Written.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    // Test "allow_session" decision (alternative to "allow").
    let payload = json!({
        "message": "Session-wide approval",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "interactive_client": true,
        "context": {
            "edge_tools": [
                { "type": "function", "function": { "name": "write_file", "description": "Write", "parameters": { "type": "object", "properties": {} } } }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    let approval_identity = wait_for_approval_identity(&mut rx).await;
    wait_for_sse(&mut rx, "approval_required", E2E_WAIT_TIMEOUT_SECS).await;
    let st = post_approval_respond(
        &app,
        &approval_identity,
        "tc-session-approve",
        "allow_session",
    )
    .await;
    assert_eq!(st, 200);

    let request = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(request["request_id"].as_str(), Some("tc-session-approve"));
    let st = post_tool_result_from_event(&app, &request, "ok", "completed").await;
    assert_eq!(st, 200);

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let text = find_events(&events, "text_delta");
    assert!(
        !text.is_empty(),
        "should complete after allow_session approval"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    assert_eq!(gateway.requests.lock().await.len(), 2);
}

// ══════════════════════════════════════════════════════════════════════════════
// PHASE A: RUN LIFECYCLE, EVENT REPLAY, STATE CONSISTENCY
// ══════════════════════════════════════════════════════════════════════════════

// ── Helpers for Phase A ──────────────────────────────────────────────────────

/// GET /chat/runs/{run_id} — returns JSON body.
async fn get_run_status(app: &Router, run_id: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/chat/runs/{run_id}"))
        .header("authorization", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// GET /chat/runs/{run_id} with a custom auth header.
async fn get_run_status_with_auth(app: &Router, run_id: &str, auth: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/chat/runs/{run_id}"))
        .header("authorization", auth)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// GET /chat/runs/{run_id}/stream?last_index=N&replay_only=true — returns the
/// durable backlog without the live-attach `session_info` envelope.
async fn get_run_stream(app: &Router, run_id: &str, last_index: u32) -> (StatusCode, Vec<Value>) {
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/chat/runs/{run_id}/stream?last_index={last_index}&replay_only=true"
        ))
        .header("authorization", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&bytes);
    let events = parse_sse_events(&body_str);
    (status, events)
}

/// GET /runs?limit=N — list runs using cursor pagination.
async fn list_runs(app: &Router, limit: u32) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/runs?limit={limit}"))
        .header("authorization", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Convenience: stream a chat, wait for completion, extract run_id.
async fn stream_and_get_run_id(app: &Router, payload: Value) -> (Vec<Value>, String, String) {
    let events = chat_stream_collect(app, payload).await;
    let si = find_events(&events, "session_info");
    assert!(!si.is_empty(), "must have session_info event");
    let run_id = si[0]
        .get("run_id")
        .and_then(Value::as_str)
        .expect("run_id in session_info")
        .to_string();
    let session_id = si[0]
        .get("session_id")
        .and_then(Value::as_str)
        .expect("session_id in session_info")
        .to_string();
    (events, run_id, session_id)
}

// ── A1: Run Status Field Verification ────────────────────────────────────────

#[tokio::test]
async fn a1_run_status_all_fields_text_only() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("a1_run_status_all_fields_text_only", |request| primary_request_for(request, "text only run"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Done."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let payload = json!({
        "message": "text only run",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let (_events, run_id, session_id) = stream_and_get_run_id(&app, payload).await;
    let body = poll_run_status(&app, &run_id, "completed", E2E_WAIT_TIMEOUT_SECS).await;

    // Verify ALL RunStatusResponse fields.
    assert_eq!(body["run_id"].as_str().unwrap(), run_id);
    assert_eq!(body["session_id"].as_str().unwrap(), session_id);
    assert_eq!(body["status"].as_str().unwrap(), "completed");
    assert!(
        body["waiting_for"].is_null(),
        "completed run should not be waiting: {:?}",
        body["waiting_for"]
    );
    let events_count = body["events_count"].as_i64().unwrap();
    assert!(
        events_count > 0,
        "events_count should be > 0, got {events_count}"
    );
    assert!(body["workspace"].is_null());
    assert!(body["executor"].is_null());
    assert!(body["transport"].is_null());
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

// ── A4: Ledger Cleanup Verification ──────────────────────────────────────────

#[tokio::test]
async fn completed_run_can_be_queried_and_replayed_by_its_owner() {
    init_env();
    let answer = "The completed run can be queried and replayed.";
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("complete then replay", |request| primary_request_for(request, "Complete this explanation."), vec![ProviderResponse::OpenAi(json!({
        "choices":[{"index":0,"message":{"role":"assistant","content":answer},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}
    }))])]).await;
    let (live, run_id, session_id) = stream_and_get_run_id(
        &app,
        json!({
            "message":"Complete this explanation.",
            "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"}
        }),
    )
    .await;
    assert_eq!(find_events(&live, "session_info").len(), 1);
    assert!(!session_id.is_empty());
    assert!(session_id.contains('-'));
    let status = poll_run_status(&app, &run_id, "completed", E2E_WAIT_TIMEOUT_SECS).await;
    let (code, queried) = get_run_status(&app, &run_id).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(queried["run_id"], run_id);
    assert_eq!(queried["session_id"], session_id);
    assert_eq!(queried["status"], "completed");
    let (code, listing) = list_runs(&app, 50).await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        listing["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == run_id && run["status"] == "completed")
    );
    let (code, _) = get_run_status_with_auth(&app, &run_id, "Bearer wrong-token").await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);

    let (code, all) = get_run_stream(&app, &run_id, 0).await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        all.len() >= 2,
        "completed run must have durable visible events"
    );
    let indices: Vec<_> = all
        .iter()
        .map(|event| event["index"].as_u64().expect("durable event index"))
        .collect();
    assert_eq!(indices[0], 0);
    // One durable terminal row projects usage and run_finished at the same index.
    assert!(indices.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(all.iter().any(|event| matches!(
        event["type"].as_str().or(event["event_type"].as_str()),
        Some("run_started" | "run_finished")
    )));

    let (code, from_one) = get_run_stream(&app, &run_id, 1).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        from_one,
        all.iter()
            .filter(|event| event["index"].as_u64().unwrap() >= 1)
            .cloned()
            .collect::<Vec<_>>(),
        "cursor refers to durable rows, including hidden rows"
    );
    let visible_cursor = *indices.iter().find(|index| **index > 0).unwrap();
    assert!(visible_cursor > 0);
    let (code, from_visible) =
        get_run_stream(&app, &run_id, u32::try_from(visible_cursor).unwrap()).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        from_visible,
        all.iter()
            .filter(|event| event["index"].as_u64().unwrap() >= visible_cursor)
            .cloned()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        from_visible.first().unwrap()["index"],
        visible_cursor,
        "cursor is inclusive"
    );
    let beyond = status["events_count"]
        .as_u64()
        .expect("durable last index plus one");
    assert!(beyond > *indices.last().unwrap());
    let (code, empty) = get_run_stream(&app, &run_id, u32::try_from(beyond).unwrap()).await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        empty.is_empty(),
        "replay beyond the durable end returns no events"
    );

    let text: String = find_events(&live, "text_delta")
        .iter()
        .map(|event| event["content"].as_str().unwrap())
        .collect();
    assert_eq!(text, answer);
    let completed: Vec<_> = all
        .iter()
        .filter(|event| event["type"] == "text_done" || event["event_type"] == "text_done")
        .collect();
    assert_eq!(completed.len(), 1);
    let full_text = completed[0]["full_text"]
        .as_str()
        .or_else(|| {
            completed[0]
                .pointer("/data/full_text")
                .and_then(Value::as_str)
        })
        .unwrap();
    assert_eq!(full_text, text);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(
        inference.attempt_count(),
        1,
        "queries and replay cannot start inference"
    );
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

// ── A5: Run Not Found / Access Denied ────────────────────────────────────────

#[tokio::test]
async fn a5_run_status_not_found() {
    init_env();
    let (app, _) = build_test_app();

    let (status, body) = get_run_status(&app, "nonexistent-run-id").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nonexistent run should 404");
    assert!(
        body["detail"].as_str().is_some(),
        "error response should have detail"
    );
}

#[tokio::test]
async fn a5_stream_run_not_found() {
    init_env();
    let (app, _) = build_test_app();

    // stream_run returns SSE, so errors come as SSE events.
    let (status, events) = get_run_stream(&app, "nonexistent-stream-id", 0).await;
    assert_eq!(status, StatusCode::OK, "SSE endpoints return 200");
    // Should have an error event.
    let error_events = find_events(&events, "error");
    assert!(
        !error_events.is_empty(),
        "should have SSE error event for nonexistent run"
    );
    let code = error_events[0]["code"].as_str().unwrap_or("");
    assert_eq!(code, "NOT_FOUND", "error code should be NOT_FOUND");
}

#[tokio::test]
async fn a6_custom_session_id_preserved() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("a6_custom_session_id_preserved", |request| primary_request_for(request, "custom session"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Custom."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let custom_sid = format!("custom-{}", uuid::Uuid::new_v4());
    let payload = json!({
        "message": "custom session",
        "session_id": &custom_sid,
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let (_events, run_id, session_id) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", E2E_WAIT_TIMEOUT_SECS).await;

    // The session_id in session_info should match our custom ID.
    assert_eq!(
        session_id, custom_sid,
        "session_info should preserve custom session_id"
    );

    // Run status should also reflect the custom session_id.
    let (_, body) = get_run_status(&app, &run_id).await;
    assert_eq!(body["session_id"].as_str().unwrap(), custom_sid);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn a6_multiple_runs_same_session() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("two serial runs", |request| primary_request_for(request, "run 1") || primary_request_for(request, "run 2"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Run one."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})), ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Run two."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let shared_sid = format!("shared-{}", uuid::Uuid::new_v4());

    // First run.
    let payload1 = json!({
        "message": "run 1",
        "session_id": &shared_sid,
    "context":{"edge_profile":{"active_skills":["concise"]}},
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });
    let (events_1, run_id_1, sid_1) = stream_and_get_run_id(&app, payload1).await;
    poll_run_status(&app, &run_id_1, "completed", E2E_WAIT_TIMEOUT_SECS).await;

    // Second run with same session.
    let payload2 = json!({
        "message": "run 2",
        "session_id": &shared_sid,
    "context":{"edge_profile":{"active_skills":["concise"]}},
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });
    let (events_2, run_id_2, sid_2) = stream_and_get_run_id(&app, payload2).await;
    poll_run_status(&app, &run_id_2, "completed", E2E_WAIT_TIMEOUT_SECS).await;

    for events in [&events_1, &events_2] {
        assert!(!find_events(events, "context_meta").is_empty());
        assert_eq!(find_events(events, "turn_complete").len(), 1);
    }
    // Both should share the same session_id.
    assert_eq!(sid_1, shared_sid);
    assert_eq!(sid_2, shared_sid);
    assert_ne!(
        run_id_1, run_id_2,
        "different runs should have different run_ids"
    );

    // Both runs should be queryable.
    let (s1, b1) = get_run_status(&app, &run_id_1).await;
    let (s2, b2) = get_run_status(&app, &run_id_2).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b1["session_id"].as_str().unwrap(), shared_sid);
    assert_eq!(b2["session_id"].as_str().unwrap(), shared_sid);
    assert_eq!(b1["status"], "completed");
    assert_eq!(b2["status"], "completed");
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 2);
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(primary_request_for(&requests[0], "run 1"));
    assert!(primary_request_for(&requests[1], "run 2"));
}

// ─── Turn Complete Event Tests ──────────────────────────────────────────────

#[tokio::test]
async fn turn_complete_is_last_typed_event() {
    init_env();
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("turn_complete_is_last_typed_event", |request| primary_request_for(request, "order check"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Done."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

    let payload = json!({
        "message": "order check",
        "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
    });

    let (events, _, _) = stream_and_get_run_id(&app, payload).await;
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();

    let tc_pos = types.iter().position(|t| *t == "turn_complete");
    assert!(
        tc_pos.is_some(),
        "turn_complete should be present, got: {types:?}"
    );
    // turn_complete should be the last event with a "type" field in the SSE stream.
    assert_eq!(
        tc_pos.unwrap(),
        types.len() - 1,
        "turn_complete should be the last typed SSE event, order: {types:?}"
    );
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

// ─── Subscriber Disconnect Tests ───────────────────────────────────

#[tokio::test]
async fn client_disconnect_run_still_finalizes() {
    let release = Arc::new(tokio::sync::Notify::new());
    let text = json!({"id":"disconnect-native","model":"test-model","choices":[{"index":0,"delta":{"content":"Completed after subscriber disconnect."},"finish_reason":null}]});
    let finish = json!({"id":"disconnect-native","model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}});
    let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new(
        "finish real inference after subscriber disconnect",
        |request| primary_request_for(request, "disconnect test"),
        vec![ProviderResponse::Stream {
            content_type: "text/event-stream",
            chunks: vec![
                format!("data: {text}\n\n").into_bytes(),
                format!("data: {finish}\n\n").into_bytes(),
                b"data: [DONE]\n\n".to_vec(),
            ],
            release_before_chunk: Some((0, release.clone())),
        }],
    )])
    .await;
    let response=chat_stream_start(&app,json!({"message":"disconnect test","execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"}})).await;
    let mut stream = response.into_body().into_data_stream();
    let mut buffer = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let run_id = loop {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("session_info deadline")
            .expect("session_info before EOF")
            .unwrap();
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(event) = parse_sse_events(&buffer)
            .into_iter()
            .find(|event| event["type"] == "session_info")
        {
            break event["run_id"].as_str().unwrap().to_string();
        }
    };
    let calls = gateway.requests.clone();
    poll_until(
        || {
            let calls = calls.clone();
            async move { calls.lock().await.len() == 1 }
        },
        5,
    )
    .await;
    let (status, running) = get_run_status(&app, &run_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(running["status"], "running");
    drop(stream);
    release.notify_one();
    let completed = poll_run_status(&app, &run_id, "completed", 5).await;
    assert_eq!(
        completed["status"], "completed",
        "disconnect is not explicit cancellation"
    );
    let (status, replay) = get_run_stream(&app, &run_id, 0).await;
    assert_eq!(status, StatusCode::OK);
    assert!(replay.iter().any(|event| event["type"] == "text_done"
        && event["full_text"] == "Completed after subscriber disconnect."));
    assert!(
        replay
            .iter()
            .any(|event| event["type"] == "run_finished" && event["status"] == "completed")
    );
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

// ── Hook DB + Observer Persistence Tests ─────────────────────────────────────

/// Ordinary answers reach the observer without writing a hook projection.
#[tokio::test]
async fn hook_db_text_only_skips_writer() {
    let gateway = ProviderGateway::start(vec![ProviderScript::new("hook_db_text_only_skips_writer", |request| primary_request_for(request, "hello"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Hi there!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, hook_writer, observer_worker, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "hello",
            "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
        }),
    )
    .await;
    assert!(!events.is_empty());

    poll_until(
        || {
            let observer = observer_worker.clone();
            async move { !observer.requests.lock().await.is_empty() }
        },
        5,
    )
    .await;
    assert!(hook_writer.plans.lock().await.is_empty());

    // Observer should have been called with messages.
    let requests = observer_worker.requests.lock().await;
    assert_eq!(requests.len(), 1, "observer fired once");
    assert_eq!(requests[0].user_id, USER_ID);
    assert!(
        requests[0]
            .messages
            .iter()
            .any(
                |message| message.get("role").and_then(Value::as_str) == Some("user")
                    && message.get("content").and_then(Value::as_str) == Some("hello")
            )
    );
    assert!(
        requests[0]
            .messages
            .iter()
            .any(
                |message| message.get("role").and_then(Value::as_str) == Some("assistant")
                    && message.get("content").and_then(Value::as_str) == Some("Hi there!")
            )
    );
    let text = find_events(&events, "text_delta")
        .into_iter()
        .map(|event| event["content"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(text, "Hi there!");
    assert_eq!(find_events(&events, "turn_complete").len(), 1);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

/// Observer receives correct session_id and turn_count.
#[tokio::test]
async fn observer_fired_with_correct_metadata() {
    let gateway = ProviderGateway::start(vec![ProviderScript::new("observer_fired_with_correct_metadata", |request| primary_request_for(request, "hello"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Hi!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, observer_worker, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "hello",
            "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
            "session_id": "obs-session-123",
        }),
    )
    .await;
    let session_info = events.iter().find(|e| e["type"] == "session_info");
    let session_id = session_info.unwrap()["session_id"].as_str().unwrap();
    assert_eq!(session_id, "obs-session-123");

    let ow = observer_worker.clone();
    poll_until(
        || {
            let ow = ow.clone();
            async move { !ow.requests.lock().await.is_empty() }
        },
        5,
    )
    .await;

    let requests = observer_worker.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].session_id, "obs-session-123");
    assert!(requests[0].turn_count >= 1, "at least one turn completed");
    let text = find_events(&events, "text_delta")
        .into_iter()
        .map(|event| event["content"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(text, "Hi!");
    assert_eq!(find_events(&events, "turn_complete").len(), 1);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn edge_callbacks_and_approval_reach_the_same_provider_execution() {
    let cases = [
        EdgeCallbackScenario {
            name: "read_file",
            message: "read the README".to_string(),
            edge_tools: vec!["read_file"],
            steps: vec![EdgeCallbackStep {
                request_id: "tc-matrix-read",
                tool_name: "read_file",
                args: json!({"path": "README.md"}),
                result_output: "README contents",
                requires_approval: false,
            }],
            final_text: "Read the README.",
        },
        EdgeCallbackScenario {
            name: "write_file_with_approval",
            message: "create a new file named notes.txt".to_string(),
            edge_tools: vec!["write_file"],
            steps: vec![EdgeCallbackStep {
                request_id: "tc-matrix-write",
                tool_name: "write_file",
                args: json!({"path": "notes.txt", "content": "hello"}),
                result_output: "file created",
                requires_approval: true,
            }],
            final_text: "Created the file.",
        },
        EdgeCallbackScenario {
            name: "search_with_grep",
            message: "search the repo for TODO".to_string(),
            edge_tools: vec!["grep"],
            steps: vec![EdgeCallbackStep {
                request_id: "tc-matrix-grep",
                tool_name: "grep",
                args: json!({"pattern": "TODO", "path": "."}),
                result_output: "src/main.rs:12:// TODO",
                requires_approval: false,
            }],
            final_text: "Found TODO matches.",
        },
        EdgeCallbackScenario {
            name: "multi_tool_batch",
            message: "inspect the project files".to_string(),
            edge_tools: vec!["read_file", "list_dir"],
            steps: vec![
                EdgeCallbackStep {
                    request_id: "tc-matrix-list",
                    tool_name: "list_dir",
                    args: json!({"path": "."}),
                    result_output: "Cargo.toml\nREADME.md",
                    requires_approval: false,
                },
                EdgeCallbackStep {
                    request_id: "tc-matrix-read-batch",
                    tool_name: "read_file",
                    args: json!({"path": "README.md"}),
                    result_output: "README contents",
                    requires_approval: false,
                },
            ],
            final_text: "Inspected the project files.",
        },
    ];

    // These are independent user/session fixtures. Exercise isolation with
    // bounded concurrency rather than adding every scenario's latency into
    // one serial deadline or flooding the shared test executor all at once.
    // Each scenario retains its own event, stream, and persistence deadlines.
    for pair in cases.chunks(2) {
        futures_util::future::join_all(pair.iter().cloned().map(run_tool_scenario)).await;
    }
}

#[tokio::test]
async fn context_meta_exposes_late_round_guidance_signals() {
    let gateway=ProviderGateway::start(vec![ProviderScript::new("context_meta_exposes_late_round_guidance_signals", |request| primary_request_for(request,"inspect the project files"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-guidance-r1", "read_file", json!({"path": "README.md"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-guidance-r2", "list_dir", json!({"path": "."}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-guidance-r3", "grep", json!({"pattern": "TODO", "path": "."}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-guidance-r4", "grep", json!({"pattern": "FIXME", "path": "."}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-guidance-r5", "glob", json!({"pattern": "**/*.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-guidance-r6", "read_file", json!({"path": "src/main.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-guidance-r7", "list_dir", json!({"path": "src"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
                            tool_call("tc-guidance-r8a", "grep", json!({"pattern": "fn main", "path": "."})),
                            tool_call("tc-guidance-r8b", "glob", json!({"pattern": "**/*.toml"}))
                        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-guidance-r9", "read_file", json!({"path": "Cargo.toml"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Done.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, _observer, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "inspect the project files",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "execution_budget": {
                "initial_turns": 10,
                "hard_turn_limit": 10
            },
            "context": {
                "edge_tools": [
                    tool_schema("read_file"),
                    tool_schema("list_dir"),
                    tool_schema("grep"),
                    tool_schema("glob")
                ]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    for (id, result) in [
        ("tc-guidance-r1", "README contents"),
        ("tc-guidance-r2", "src\nREADME.md"),
        ("tc-guidance-r3", "src/main.rs:12:// TODO"),
        ("tc-guidance-r4", "src/lib.rs:5:// FIXME"),
        ("tc-guidance-r5", "src/main.rs\nsrc/lib.rs"),
        ("tc-guidance-r6", "fn main() {}"),
        ("tc-guidance-r7", "main.rs\nlib.rs"),
        ("tc-guidance-r8a", "src/main.rs:1:fn main"),
        ("tc-guidance-r8b", "Cargo.toml"),
        ("tc-guidance-r9", "[package]\nname = \"astra\""),
    ] {
        let request = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
        assert_eq!(request["request_id"].as_str(), Some(id));
        let status = post_tool_result_from_event(&app, &request, result, "completed").await;
        assert_eq!(status, StatusCode::OK);
    }

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let context_meta_events = find_events(&events, "context_meta");
    assert!(
        !context_meta_events.is_empty(),
        "expected at least one context_meta event"
    );
    // Use the last context_meta event (most representative of late-round state).
    let late_round_context = context_meta_events.last().unwrap();

    assert!(
        late_round_context["system_prompt_tokens"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "context_meta should expose prompt token estimates"
    );
    let guidance_signals = &late_round_context["system_prompt_breakdown"]["guidance_signals"];
    assert!(
        guidance_signals["parallel_batching_nudge"].is_boolean(),
        "context_meta should expose the parallel_batching_nudge flag"
    );
    assert!(
        guidance_signals["parallel_feedback"].is_boolean(),
        "context_meta should expose the parallel_feedback flag"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 10);
    assert_eq!(gateway.requests.lock().await.len(), 10);
}

#[tokio::test]
async fn analysis_turn_records_circuit_breaker_advisory_without_aborting_repetition() {
    let gateway=ProviderGateway::start(vec![ProviderScript::new("analysis_turn_records_circuit_breaker_advisory_without_aborting_repetition", |request| primary_request_for(request,"review 最新的commit"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-analysis-r1", "grep", json!({"pattern": "TODO", "path": "src/"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-analysis-r2", "grep", json!({"pattern": "TODO", "path": "src/"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-analysis-r3", "grep", json!({"pattern": "TODO", "path": "src/"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-analysis-r4", "grep", json!({"pattern": "TODO", "path": "src/"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Done reviewing.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, observer_worker, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "review 最新的commit",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "execution_budget": {
                "initial_turns": 20,
                "hard_turn_limit": 20
            },
            "context": {
                "edge_tools": [
                    tool_schema("grep"),
                    tool_schema("list_dir"),
                    tool_schema("read_file"),
                    tool_schema("glob")
                ]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Drive every callback the runtime actually dispatches. Identical
    // read-only calls may reuse the first result, so logical tool rounds and
    // physical edge callbacks intentionally have different cardinalities.
    let mut callback_request_ids = Vec::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("stream made no progress while awaiting a tool callback");
        let Some(event) = event else { break };
        if event.get("type").and_then(Value::as_str) != Some("tool_request") {
            continue;
        }
        let request_id = event["request_id"]
            .as_str()
            .expect("tool_request.request_id")
            .to_string();
        let status =
            post_tool_result_from_event(&app, &event, "src/lib.rs:12:// TODO", "completed").await;
        assert_eq!(status, StatusCode::OK);
        callback_request_ids.push(request_id);
    }

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    assert_eq!(
        callback_request_ids.first().map(String::as_str),
        Some("tc-analysis-r1"),
        "the first logical call must be dispatched before results can be reused"
    );
    assert!(
        find_events(&events, "text_done")
            .iter()
            .any(|event| event["full_text"].as_str() == Some("Done reviewing.")),
        "the repeated investigation should reach its final answer"
    );

    let ow = observer_worker.clone();
    poll_until(
        move || {
            let ow = ow.clone();
            async move { !ow.requests.lock().await.is_empty() }
        },
        5,
    )
    .await;

    let requests = observer_worker.requests.lock().await;
    assert_eq!(
        requests.len(),
        1,
        "observer should fire once for the completed turn"
    );
    // Repetition is advisory evidence, not an execution boundary. The fourth
    // identical call must complete so a valid investigation is never stopped
    // merely because its tool shape repeats.
    let tool_result_count = requests[0]
        .messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .count();
    assert_eq!(
        tool_result_count, 4,
        "circuit-breaker advice must not abort a repeated but otherwise valid tool phase; got {tool_result_count}"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 5);
    assert_eq!(gateway.requests.lock().await.len(), 5);
}

#[tokio::test]
async fn execution_budget_extends_web_agent_run_when_progress_is_real() {
    let gateway=ProviderGateway::start(vec![ProviderScript::new("execution_budget_extends_web_agent_run_when_progress_is_real", |request| primary_request_for(request,"explore the codebase and investigate the root cause"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-budget-r1", "read_file", json!({"path": "src/lib.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-budget-r2", "glob", json!({"pattern": "src/**/*.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Completed after exploratory extension.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, _observer, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "explore the codebase and investigate the root cause",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "execution_budget": {
                "initial_turns": 2,
                "hard_turn_limit": 4
            },
            "context": {
                "edge_tools": [
                    tool_schema("read_file"),
                    tool_schema("glob")
                ]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    let first = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(first["request_id"].as_str(), Some("tc-budget-r1"));
    assert_eq!(
        post_tool_result_from_event(&app, &first, "module contents", "completed").await,
        StatusCode::OK
    );

    let second = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(second["request_id"].as_str(), Some("tc-budget-r2"));
    assert_eq!(
        post_tool_result_from_event(&app, &second, "src/lib.rs\nsrc/main.rs", "completed").await,
        StatusCode::OK
    );

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    let text: String = find_events(&events, "text_delta")
        .into_iter()
        .filter_map(|event| event["content"].as_str().map(str::to_string))
        .collect();
    assert!(
        text.contains("Completed after exploratory extension."),
        "expected post-extension final text, got: {text}"
    );
    assert!(
        !text.contains("Turn budget exhausted"),
        "extension path should not terminate with exhaustion text: {text}"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 3);
    assert_eq!(gateway.requests.lock().await.len(), 3);
}

#[tokio::test]
async fn execution_budget_hard_limit_stops_web_agent_run_even_with_progress() {
    let gateway=ProviderGateway::start(vec![ProviderScript::new("execution_budget_hard_limit_stops_web_agent_run_even_with_progress", |request| primary_request_for(request,"explore the codebase and investigate the root cause"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-hard-limit-r1", "read_file", json!({"path": "src/lib.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-hard-limit-r2", "glob", json!({"pattern": "src/**/*.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Final answer after the hard limit.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, _observer, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "explore the codebase and investigate the root cause",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "execution_budget": {
                "initial_turns": 2,
                "hard_turn_limit": 2
            },
            "context": {
                "edge_tools": [
                    tool_schema("read_file"),
                    tool_schema("glob")
                ]
            }
        }),
    )
    .await;
    let raw_wire = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = raw_wire.clone();
    let body = Body::from_stream(resp.into_body().into_data_stream().inspect(move |chunk| {
        if let Ok(bytes) = chunk {
            capture.lock().unwrap().extend_from_slice(bytes);
        }
    }));
    let (mut rx, reader) = spawn_sse_reader(body).await;

    let first = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(first["request_id"].as_str(), Some("tc-hard-limit-r1"));
    assert_eq!(
        post_tool_result_from_event(&app, &first, "module contents", "completed").await,
        StatusCode::OK
    );

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    let wire = String::from_utf8(raw_wire.lock().unwrap().clone()).unwrap();
    let interrupted = events
        .iter()
        .position(|event| event["type"] == "run_interrupted")
        .expect("typed interruption");
    let finished = events
        .iter()
        .position(|event| event["type"] == "run_finished")
        .expect("durable terminal");
    let complete = events
        .iter()
        .position(|event| event["type"] == "turn_complete")
        .expect("authoritative completion");
    assert!(interrupted < finished && finished < complete);
    assert_eq!(events[interrupted]["kind"], "budget_exhausted");
    assert_eq!(events[complete]["continuation_owner"], "server");
    assert_eq!(events[complete]["interruption"]["kind"], "budget_exhausted");
    assert_eq!(wire.matches("data: [DONE]\n\n").count(), 1);
    let mut accum = ChatTurnSseAccum::default();
    let mut edge_pending = Vec::new();
    for block in wire.split("\n\n").filter(|block| !block.is_empty()) {
        dispatch_chat_turn_sse_event_block(block, &mut accum, &mut edge_pending);
    }
    assert!(accum.server_loop_terminal);
    assert_eq!(accum.error_kind, None);
    let summary = accum
        .server_execution_summary
        .expect("CLI accepts the actual paused summary");
    assert_eq!(summary.llm_rounds, 3);
    let feedback = summary
        .runtime_feedback
        .expect("actual provider execution supplies runtime feedback");
    assert_eq!(
        feedback.identity.run_id,
        find_event(&events, "session_info").unwrap()["run_id"]
            .as_str()
            .unwrap()
    );
    assert_eq!(feedback.progress.llm_rounds_completed, 3);
    assert_eq!(feedback.progress.slice_rounds_remaining, 0);
    assert_eq!(feedback.progress.absolute_round_ceiling, Some(2));
    let accounted = feedback
        .run_usage
        .expect("settled provider attempts retain their usage");
    assert_eq!(accounted.prompt, 126);
    assert_eq!(accounted.completion, 21);
    let root = find_event(&events, "session_info").unwrap()["run_id"]
        .as_str()
        .unwrap();
    let (status, durable) = get_run_status(&app, root).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(durable["status"], "paused");
    let text: String = find_events(&events, "text_delta")
        .into_iter()
        .filter_map(|event| event["content"].as_str().map(str::to_string))
        .collect();
    assert!(
        text.contains("Final answer after the hard limit."),
        "hard limit should preserve the text-only settlement boundary, got: {text}"
    );
    let tool_requests = find_events(&events, "tool_request");
    assert_eq!(
        tool_requests.len(),
        1,
        "hard limit must dispatch exactly one tool request before text-only settlement"
    );
    assert_eq!(
        tool_requests[0]["request_id"].as_str(),
        Some("tc-hard-limit-r1")
    );
    let rejected = find_events(&events, "tool_call_end")
        .into_iter()
        .find(|event| event["call_id"] == "tc-hard-limit-r2")
        .expect("the denied second provider attempt must have one terminal projection");
    assert_eq!(rejected["status"], "rejected");
    assert_eq!(rejected["success"], false);

    let terminal = find_events(&events, "turn_complete")
        .into_iter()
        .next()
        .expect("bounded synthesis must emit one terminal aggregate");
    assert_eq!(terminal["tool_calls_count"], 2);
    assert_eq!(terminal["tool_ledger_receipt"]["attempted"], 2);
    assert_eq!(terminal["tool_ledger_receipt"]["terminal"], 2);
    assert_eq!(
        terminal["tool_ledger_receipt"]["result_classes"]["rejected"],
        1
    );
    assert_eq!(terminal["tool_ledger_receipt"]["unresolved"], 0);
    assert_eq!(terminal["tool_ledger_receipt"]["consistent"], true);
    let run_finished = find_events(&events, "run_finished")
        .into_iter()
        .next()
        .expect("run terminal");
    assert_eq!(terminal["execution_state"]["status"], "interrupted");
    assert_eq!(
        terminal["execution_state"]["interruption_kind"],
        "budget_exhausted"
    );
    assert_eq!(run_finished["status"], "paused");
    assert_eq!(run_finished["interruption_kind"], "budget_exhausted");
    assert_eq!(run_finished["resumable"], true);

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 3);
    assert_eq!(gateway.requests.lock().await.len(), 3);
}

#[tokio::test]
async fn web_agent_stream_preserves_failed_edge_statuses_in_tool_call_end() {
    let gateway=ProviderGateway::start(vec![ProviderScript::new("web_agent_stream_preserves_failed_edge_statuses_in_tool_call_end", |request| primary_request_for(request,"explore the codebase and investigate the root cause"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-budget-fail-r1", "read_file", json!({"path": "src/lib.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[tool_call("tc-budget-fail-r2", "glob", json!({"pattern": "src/**/*.rs"}))]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Edge operations returned partial failure and denial.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, _observer, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "explore the codebase and investigate the root cause",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "execution_budget": {
                "initial_turns": 2,
                "hard_turn_limit": 4
            },
            "context": {
                "edge_tools": [
                    tool_schema("read_file"),
                    tool_schema("glob")
                ]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    let first = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(first["request_id"].as_str(), Some("tc-budget-fail-r1"));
    assert_eq!(
        post_tool_result_from_event(&app, &first, "transient read failure", "partial_failure")
            .await,
        StatusCode::OK
    );

    let second = wait_for_sse(&mut rx, "tool_request", E2E_WAIT_TIMEOUT_SECS).await;
    assert_eq!(second["request_id"].as_str(), Some("tc-budget-fail-r2"));
    assert_eq!(
        post_tool_result_from_event(&app, &second, "permission denied", "denied").await,
        StatusCode::OK
    );

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    let tool_end_results: Vec<String> = find_events(&events, "tool_call_end")
        .into_iter()
        .filter_map(|event| event["result"].as_str().map(str::to_string))
        .collect();
    assert!(
        tool_end_results
            .iter()
            .any(|result| result.contains("status=partial_failure")),
        "expected partial_failure tool result in SSE stream, got: {tool_end_results:?}"
    );
    assert!(
        tool_end_results
            .iter()
            .any(|result| result.contains("status=denied")),
        "expected denied tool result in SSE stream, got: {tool_end_results:?}"
    );

    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 3);
    assert_eq!(gateway.requests.lock().await.len(), 3);
}

#[tokio::test]
async fn context_meta_exposes_memory_signal_context_flag() {
    let gateway = ProviderGateway::start(vec![ProviderScript::new("context_meta_exposes_memory_signal_context_flag", |request| primary_request_for(request, "remember that I prefer dark mode"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Stored."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, _observer, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "remember that I prefer dark mode",
            "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
        }),
    )
    .await;

    // Memory signal detection removed — LLM decides via system prompt rules.
    // Verify the flag is now always false.
    let context_meta = find_events(&events, "context_meta")
        .into_iter()
        .find(|event| {
            event["system_prompt_breakdown"]["context_signals"]["memory_signal_detected"].as_bool()
                == Some(false)
        })
        .expect("context_meta event with memory_signal_detected=false");

    assert_eq!(
        context_meta["system_prompt_breakdown"]["context_signals"]["memory_signal_detected"]
            .as_bool(),
        Some(false),
        "memory_signal_detected must be false — keyword detection removed"
    );
    assert!(
        context_meta["system_prompt_breakdown"]["context_signals"]["active_output_skills"]
            .is_boolean(),
        "context_meta should expose the other context flags as structured booleans"
    );
    let text = find_events(&events, "text_delta")
        .into_iter()
        .map(|event| event["content"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(text, "Stored.");
    assert_eq!(find_events(&events, "turn_complete").len(), 1);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn context_meta_exposes_builder_supplied_context_signals() {
    let gateway = ProviderGateway::start(vec![ProviderScript::new("context_meta_exposes_builder_supplied_context_signals", |request| primary_request_for(request, "remember that I prefer dark mode"), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Stored."},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let inference = InferenceLedgerFixture::default();
    let (app, _hook_writer, _observer, _ledger) = build_test_app_with_hooks(
        Arc::new(TestModelService {
            judgment_base_url: Some(format!("{}/v1", gateway.base_url)),
        }),
        Some(&inference),
    );

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "remember that I prefer dark mode",
            "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
            "context": {
                "edge_profile": {
                    "active_skills": ["concise"],
                    "system_prompt_override": "You are operating under a delegated reviewer contract."
                }
            }
        }),
    )
    .await;

    let context_meta = find_events(&events, "context_meta")
        .into_iter()
        .find(|event| event["system_prompt_breakdown"].is_object())
        .expect("builder-supplied context_meta event");

    let breakdown = &context_meta["system_prompt_breakdown"];
    assert_eq!(
        breakdown["context_signals"]["system_prompt_override"].as_bool(),
        Some(true),
        "server loop context_meta must report edge_profile.system_prompt_override"
    );
    let text = find_events(&events, "text_delta")
        .into_iter()
        .map(|event| event["content"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(text, "Stored.");
    assert_eq!(find_events(&events, "turn_complete").len(), 1);
    gateway.assert_complete();
    inference.assert_quiescent();
    assert_eq!(inference.attempt_count(), 1);
    assert_eq!(gateway.requests.lock().await.len(), 1);
}

/// Regression: `model` override must not break requests carrying `active_skills`.
///
/// The bridge marks `routing_meta.status = "skipped"` with reason `model_override`
/// whenever the caller pins a model. `active_skills` names remain accepted as
/// runtime metadata, but #629 deliberately keeps that diagnostic summary out of
/// the model prompt; actual skill instructions are delivered only after a skill
/// invocation. This test guards the request/event flow from model-override drift.
#[tokio::test]
async fn context_meta_active_skills_survive_model_override() {
    // Bound the real provider request and complete Server execution.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        init_env();
        let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("context_meta_active_skills_survive_model_override", |request| request.path == "/v1/chat/completions" && request.body["model"] == "MiniMax-M2.7" && request.body["stream"] == true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|value| value["role"] == "user" && value["content"] == "help me review")), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

        let events = chat_stream_collect(
            &app,
            json!({
                "message": "help me review",
                "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
                "model_selection": { "offering_id": "model-MiniMax-M2.7" },
                "context": {
                    "edge_profile": {
                        "active_skills": ["concise", "markdown"]
                    }
                }
            }),
        )
        .await;

        // With pipeline-based assembly, context_signals and skills_injected
        // are not individually populated in the breakdown (the pipeline
        // serializer doesn't track per-section trace signals). Verify that:
        //   1. A context_meta event IS emitted even when model override is set
        //   2. The turn completes successfully (model override doesn't break flow)
        let context_metas = find_events(&events, "context_meta");
        assert!(
            !context_metas.is_empty(),
            "context_meta must be emitted even with model_override set"
        );
        // The turn must complete — model override should not break execution.
        let turn_complete = find_events(&events, "turn_complete");
        assert!(
            !turn_complete.is_empty(),
            "turn must complete with model_override + active_skills"
        );
        let text = find_events(&events, "text_delta").into_iter().map(|event| event["content"].as_str().unwrap()).collect::<String>();
        assert_eq!(text, "ok");
        assert_eq!(turn_complete.len(), 1);
        gateway.assert_complete();
        inference.assert_quiescent();
        assert_eq!(inference.attempt_count(), 1);
        let requests = gateway.requests.lock().await;
        assert_eq!(requests.len(), 1);

    })
    .await
    .expect("context_meta_active_skills_survive_model_override exceeded 30s timeout — likely a hang regression");
}

/// Unknown skill names in runtime metadata must not crash the turn.
///
/// They are not projected into the model prompt or reported as injected skill
/// tokens; actual skill resolution remains owned by the skill invocation path.
#[tokio::test]
async fn context_meta_surfaces_unknown_active_skills_for_debugging() {
    // Bound the real provider request and complete Server execution.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        init_env();
        let (app, gateway, inference) = build_native_test_app(vec![ProviderScript::new("context_meta_surfaces_unknown_active_skills_for_debugging", |request| request.path == "/v1/chat/completions" && request.body["model"] == "test-model" && request.body["stream"] == true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|value| value["role"] == "user" && value["content"] == "test")), vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;

        let events = chat_stream_collect(
            &app,
            json!({
                "message": "test",
                "execution_policy": {"turn_intent":"fixed_default", "skill_auto_route":"disabled"},
                "context": {
                    "edge_profile": {
                        "active_skills": ["totally-nonexistent-skill-xyz"]
                    }
                }
            }),
        )
        .await;

        // With pipeline-based assembly, context_signals and skills_injected
        // are not individually populated in the breakdown. Verify that:
        //   1. A context_meta event IS emitted even with unknown skill names
        //   2. The turn completes (unknown skills don't crash execution)
        let context_metas = find_events(&events, "context_meta");
        assert!(
            !context_metas.is_empty(),
            "context_meta should fire even with unknown skill names"
        );
        // The turn must complete without error.
        let turn_complete = find_events(&events, "turn_complete");
        assert!(
            !turn_complete.is_empty(),
            "turn must complete even with unknown active_skills"
        );
        let text = find_events(&events, "text_delta").into_iter().map(|event| event["content"].as_str().unwrap()).collect::<String>();
        assert_eq!(text, "ok");
        assert_eq!(turn_complete.len(), 1);
        gateway.assert_complete();
        inference.assert_quiescent();
        assert_eq!(inference.attempt_count(), 1);
        let requests = gateway.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let system = requests[0].body["messages"].as_array().unwrap().iter().filter(|value| value["role"] == "system").map(|value| value["content"].to_string()).collect::<String>();
        assert!(!system.contains("totally-nonexistent-skill-xyz"));
    })
    .await
    .expect("context_meta_surfaces_unknown_active_skills_for_debugging exceeded 30s timeout — likely a hang regression");
}
