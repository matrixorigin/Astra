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
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

pub type EdgeCallbackLedger = Arc<tokio::sync::Mutex<HashMap<String, Value>>>;

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
                pricing: None,
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
