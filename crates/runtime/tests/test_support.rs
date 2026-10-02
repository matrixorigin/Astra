#![allow(dead_code)]

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, OnceLock},
};

use astra_core::ErrorResponse;
use astra_runtime::{
    AgenticRunLifecycleService, FernetTokenEncryptor, MatrixOneSettings, RunEngine,
};
use astra_services::{
    InMemoryRunStateStore, ModelCreateRequestData, ModelListItem, ModelRecord, ModelService,
    ModelUpdateRequestData, ResolvedActiveLlmModel, ResolvedModelOffering,
};
use async_trait::async_trait;
use axum::{Json, Router, body::Body, http::StatusCode, response::Response, routing::post};
use serde_json::{Value, json};
use uuid::Uuid;

pub type EdgeCallbackLedger = Arc<tokio::sync::Mutex<HashMap<String, Value>>>;

/// A loopback provider for real JSON/SSE judgment decoding and admission.
/// Its candidate-independent response is fixture evidence, not model-quality proof.
pub struct DelegationJudgmentProvider {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for DelegationJudgmentProvider {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl DelegationJudgmentProvider {
    pub async fn start() -> Self {
        async fn judgment(
            axum::extract::State(requests): axum::extract::State<Arc<Mutex<Vec<Value>>>>,
            Json(request): Json<Value>,
        ) -> Response {
            let is_assessment = request["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    let Some(content) = message["content"].as_str() else {
                        return false;
                    };
                    serde_json::from_str::<Value>(content).is_ok_and(|input| {
                        input["user_text"].is_string()
                            && input["candidates"].is_array()
                            && input["slots"].is_array()
                    })
                })
            });
            let model = request["model"]
                .as_str()
                .expect("requested model")
                .to_owned();
            let streaming = request["stream"] == true;
            // Only candidate assessments belong to this assertion ledger.
            // Other auxiliary schemas reject the same not_applicable payload;
            // a transport error here would trip the shared provider breaker.
            if is_assessment {
                requests.lock().unwrap().push(request);
            }
            if !streaming {
                return Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(json!({
                        "id": "offline-delegation-judgment", "object": "chat.completion",
                        "model": model,
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": "{\"disposition\":\"not_applicable\"}"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 32, "completion_tokens": 8, "total_tokens": 40}
                    }).to_string()))
                    .unwrap();
            }
            // Tasks contain no model/reasoning controls. The canonical parser
            // still binds this response to authenticated source and slot evidence.
            let mut wire = String::new();
            for content in ["{\"disposition\":", "\"not_applicable\"}"] {
                let chunk = json!({
                    "id": "offline-delegation-judgment", "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]
                });
                wire.push_str(&format!("data: {chunk}\n\n"));
            }
            let terminal = json!({
                "id": "offline-delegation-judgment", "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 32, "completion_tokens": 8, "total_tokens": 40}
            });
            wire.push_str(&format!("data: {terminal}\n\ndata: [DONE]\n\n"));
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from(wire))
                .unwrap()
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let gateway = Router::new()
            .route("/v1/chat/completions", post(judgment))
            .with_state(requests.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, gateway)
                .await
                .expect("serve offline judgment provider");
        });
        Self {
            base_url: format!("http://{address}/v1"),
            requests,
            server,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn assert_request_count(&self, expected: usize) {
        assert_eq!(self.requests.lock().unwrap().len(), expected);
    }

    /// Match concurrent roots by authenticated text, never request arrival order.
    /// Returns catalog size so single-model fixtures retain their exact oracle.
    pub fn assert_request(
        &self,
        user_text: &str,
        offering_id: &str,
        model_name: &str,
        slots: &[(&str, &str)],
        streaming: bool,
    ) -> usize {
        let requests = self.requests.lock().unwrap();
        let inputs = requests
            .iter()
            .filter_map(|request| {
                let input = request["messages"].as_array()?.iter().find_map(|message| {
                    let input: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
                    (input["user_text"] == user_text).then_some(input)
                })?;
                assert_eq!(request["model"], model_name);
                assert_eq!(
                    request["stream"] == true,
                    streaming,
                    "fixture's provider protocol"
                );
                Some(input)
            })
            .collect::<Vec<_>>();
        assert_eq!(inputs.len(), 1, "one canonical judgment per root batch");
        let input = &inputs[0];
        assert_eq!(input["user_text"], user_text);
        let candidates = input["candidates"]
            .as_array()
            .expect("authorized candidates");
        let selected = candidates
            .iter()
            .filter(|candidate| candidate["offering_id"] == offering_id)
            .collect::<Vec<_>>();
        assert_eq!(selected.len(), 1, "seeded Offering in authorized catalog");
        assert_eq!(selected[0]["model_name"], model_name);
        assert_eq!(selected[0]["provider"], "openai");
        let actual_slots = input["slots"].as_array().expect("canonical task slots");
        assert_eq!(actual_slots.len(), slots.len());
        for (index, (description, prompt)) in slots.iter().enumerate() {
            assert_eq!(actual_slots[index]["index"], index);
            assert_eq!(actual_slots[index]["description"], *description);
            assert_eq!(actual_slots[index]["prompt"], *prompt);
        }
        candidates.len()
    }
}

static SHARED_DB_TEST_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static SHARED_DB_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Run a live-database integration case on one process-owned Tokio runtime.
///
/// A SQLx pool must not outlive the runtime that owns its sockets and
/// maintenance tasks. Integration-test binaries that cache one pool across
/// cases use this runner so every case shares the same long-lived runtime,
/// matching the production server's ownership topology. The suite-wide lock
/// also keeps recovery/lease cases from claiming each other's UUID-scoped
/// active rows when libtest schedules cases concurrently.
pub fn run_shared_db_test(future: impl Future<Output = ()>) {
    let _serial = SHARED_DB_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    SHARED_DB_TEST_RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("astra-db-test")
                .build()
                .expect("shared database test runtime")
        })
        .block_on(future);
}

#[macro_export]
macro_rules! shared_db_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        #[test]
        $(#[$meta])*
        fn $name() {
            $crate::test_support::run_shared_db_test(async $body);
        }
    };
}

pub fn test_model_service(offering_id: &str, model_name: &str) -> Arc<dyn ModelService> {
    Arc::new(StaticTestModelService {
        offering_id: offering_id.to_string(),
        model_name: model_name.to_string(),
    })
}

struct StaticTestModelService {
    offering_id: String,
    model_name: String,
}

fn unsupported_model_service_call<T>() -> Result<T, (StatusCode, Json<ErrorResponse>)> {
    Err(astra_core::error_response_coded(
        StatusCode::NOT_IMPLEMENTED,
        "operation is outside the static test model service contract",
        "test_model_service_operation_unsupported",
    ))
}

#[async_trait]
impl ModelService for StaticTestModelService {
    async fn create_model(
        &self,
        _: String,
        _: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unsupported_model_service_call()
    }

    async fn list_models(
        &self,
        _: String,
        _: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        unsupported_model_service_call()
    }

    async fn get_model(&self, _: String) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unsupported_model_service_call()
    }

    async fn resolve_model_offering(
        &self,
        offering_id: String,
    ) -> Result<ResolvedModelOffering, (StatusCode, Json<ErrorResponse>)> {
        if offering_id != self.offering_id {
            return Err(astra_core::error_response_coded(
                StatusCode::NOT_FOUND,
                "test Offering is not available",
                "model_offering_not_found",
            ));
        }
        Ok(ResolvedModelOffering {
            offering_id,
            model: ResolvedActiveLlmModel {
                price_snapshot: None,
                model_name: self.model_name.clone(),
                wire_model_name: None,
                api_key: "test-key".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                provider: "mock".to_string(),
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
    }

    async fn update_model(
        &self,
        _: String,
        _: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unsupported_model_service_call()
    }

    async fn delete_model(&self, _: String) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unsupported_model_service_call()
    }

    async fn check_model(
        &self,
        _: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unsupported_model_service_call()
    }
}

pub fn test_fernet_encryptor(key: &str) -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new(key).expect("fernet key"))
}

pub fn test_matrixone_settings() -> MatrixOneSettings {
    MatrixOneSettings {
        host: "127.0.0.1".into(),
        port: 0,
        user: "x".into(),
        password: "x".into(),
        database: "x".into(),
        db_pool_max_connections: 1,
        db_pool_min_connections: 1,
        db_pool_acquire_timeout_secs: 5,
        db_pool_idle_timeout_secs: 60,
        db_pool_max_lifetime_secs: 300,
    }
}

pub fn require_db_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    MatrixOneSettings::from_env()
}

pub fn test_run_lifecycle(
    encryptor: Arc<FernetTokenEncryptor>,
    ledger: EdgeCallbackLedger,
) -> AgenticRunLifecycleService {
    let run_engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    AgenticRunLifecycleService::new(test_matrixone_settings(), encryptor, ledger, run_engine)
}

pub fn tool_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": serde_json::to_string(&args).unwrap()
        }
    })
}

pub fn tool_schema(name: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": format!("{name} tool"),
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }
        }
    })
}

pub fn parse_sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

pub fn assert_contract_json(actual: &Value, expected: &Value, label: &str) {
    if let Some(expected_obj) = expected.as_object()
        && expected_obj.contains_key("detail")
        && !expected_obj.contains_key("request_id")
    {
        let actual_obj = actual
            .as_object()
            .unwrap_or_else(|| panic!("{label}: actual response should be a JSON object"));
        let request_id = actual_obj
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{label}: error response should include request_id"));
        assert!(
            Uuid::parse_str(request_id).is_ok(),
            "{label}: request_id should be a UUID"
        );

        let mut normalized_actual = actual_obj.clone();
        normalized_actual.remove("request_id");
        assert_eq!(Value::Object(normalized_actual), *expected, "{label}");
        return;
    }

    assert_eq!(actual, expected, "{label}");
}
