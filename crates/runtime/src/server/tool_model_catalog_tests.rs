use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use astra_core::{ErrorResponse, error_response};
use astra_services::{auth::*, models::*};
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};

use super::handle_model_catalog;

type HttpResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

#[derive(Default)]
struct CatalogSpy {
    reads: AtomicUsize,
    items: Mutex<Vec<ModelListItem>>,
    failure: Mutex<Option<StatusCode>>,
}

#[async_trait]
impl ModelService for CatalogSpy {
    async fn user_model_catalog(&self, user: String) -> HttpResult<UserModelCatalog> {
        assert_eq!(user, "owner");
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(status) = *self.failure.lock().unwrap() {
            return Err(error_response(
                status,
                "private backend endpoint and credential must not escape",
            ));
        }
        Ok(UserModelCatalog {
            items: self.items.lock().unwrap().clone(),
            default_offering_id: None,
            allows_deployment: false,
        })
    }
    async fn model_catalog_revision(&self, _: String, _: bool) -> HttpResult<String> {
        panic!("revision must be pure")
    }
    async fn create_model(&self, _: String, _: ModelCreateRequestData) -> HttpResult<ModelRecord> {
        panic!("not discovery")
    }
    async fn list_models(&self, _: String, _: bool) -> HttpResult<Vec<ModelListItem>> {
        panic!("must use catalog owner exactly once")
    }
    async fn get_model(&self, _: String) -> HttpResult<ModelRecord> {
        panic!("not discovery")
    }
    async fn resolve_model_offering(&self, _: String) -> HttpResult<ResolvedModelOffering> {
        panic!("discovery cannot load credentials")
    }
    async fn update_model(&self, _: String, _: ModelUpdateRequestData) -> HttpResult<ModelRecord> {
        panic!("not discovery")
    }
    async fn delete_model(&self, _: String) -> HttpResult<()> {
        panic!("not discovery")
    }
    async fn check_model(&self, _: String) -> HttpResult<ModelRecord> {
        panic!("not discovery")
    }
}

#[derive(Default)]
struct ScopedAuthSpy {
    reads: AtomicUsize,
    failure: Mutex<Option<StatusCode>>,
}

#[async_trait]
impl AuthService for ScopedAuthSpy {
    async fn external_catalog_by_scope(
        &self,
        principal: &AuthPrincipal,
    ) -> HttpResult<ExternalCatalogResponse> {
        assert!(principal.is_edge_registration());
        let AuthPrincipalOrigin::ProviderAuthorizedRequest(context) = &principal.origin else {
            panic!("wrong scope")
        };
        assert_eq!(context.provider_scope_id, "restricted-scope");
        assert_eq!(context.external_subject, "subject");
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(status) = *self.failure.lock().unwrap() {
            return Err(error_response(status, "private provider failure"));
        }
        Ok(serde_json::from_value(json!({"models":[{
            "id":"scoped-offering", "name":"scoped-name", "model_name":"wire-name", "provider_ref":"openai",
            "metadata":{"api_key":"private-key", "endpoint":"private-endpoint", "description":"private-description"}
        }]})).unwrap())
    }
    async fn register(&self, _: AuthRegisterRequestData) -> HttpResult<AuthUserRecord> {
        panic!("not catalog")
    }
    async fn login(&self, _: AuthLoginRequestData) -> HttpResult<AuthTokenRecord> {
        panic!("not catalog")
    }
    async fn refresh(&self, _: AuthRefreshRequestData) -> HttpResult<AuthTokenRecord> {
        panic!("not catalog")
    }
    async fn logout(&self, _: AuthRefreshRequestData) -> HttpResult<()> {
        panic!("not catalog")
    }
    async fn current_user(&self, _: &HeaderMap) -> HttpResult<AuthUserRecord> {
        panic!("principal must already be bound")
    }
}

fn principal(edge: bool) -> AuthPrincipal {
    AuthPrincipal {
        user: AuthUserRecord {
            user_id: "owner".into(),
            username: "owner".into(),
            email: "".into(),
            display_name: None,
        },
        session_id: None,
        origin: if edge {
            AuthPrincipalOrigin::ProviderAuthorizedRequest(AuthProviderAuthorizedRequestContext {
                provider_id: "provider".into(),
                external_subject: "subject".into(),
                provider_scope_id: "restricted-scope".into(),
                request_authorization_id: "authorization".into(),
                edge_agent_id: Some("edge".into()),
            })
        } else {
            AuthPrincipalOrigin::Internal
        },
    }
}

fn fixture(index: usize) -> ModelListItem {
    ModelListItem {
        offering_id: format!("id-{index:03}"),
        name: format!("name-{index:03}"),
        provider: "openai".into(),
        access_id: "owner-access".into(),
        access_kind: ModelAccessKind::CloudByok,
        access_label: "Personal".into(),
        execution_placement: ModelExecutionPlacement::Server,
        description: Some("private-description".into()),
        is_active: true,
        context_window: 32768,
        max_completion_tokens: None,
        architecture: None,
        thinking_capability: None,
        pricing: None,
    }
}

#[tokio::test]
async fn edge_scope_never_reads_owner_catalog_even_when_revoked() {
    let models = Arc::new(CatalogSpy::default());
    *models.items.lock().unwrap() = vec![fixture(0)];
    let auth = Arc::new(ScopedAuthSpy::default());
    let reader = AuthorizedModelCatalogReader::new(models.clone(), auth.clone(), principal(true));
    let child = reader.clone();
    let result = handle_model_catalog(&json!({}), "owner", Some(&child)).await;
    let value: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(value["principal_scope"], "edge_registration");
    assert_eq!(value["items"][0]["offering_id"], "scoped-offering");
    assert!(!result.output.contains("private"));
    assert_eq!(models.reads.load(Ordering::SeqCst), 0);
    assert_eq!(auth.reads.load(Ordering::SeqCst), 1);
    *auth.failure.lock().unwrap() = Some(StatusCode::FORBIDDEN);
    let denied = handle_model_catalog(&json!({}), "owner", Some(&child)).await;
    let value: Value = serde_json::from_str(&denied.output).unwrap();
    assert!(denied.is_error);
    assert_eq!(value["error"]["error_kind"], "unauthorized");
    assert_eq!(value["error"]["retryable"], false);
    assert!(value["total"].is_null());
    assert_eq!(models.reads.load(Ordering::SeqCst), 0);
    assert_eq!(auth.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unbound_wrong_owner_and_backend_failures_have_safe_errors() {
    let models = Arc::new(CatalogSpy::default());
    let reader = AuthorizedModelCatalogReader::new(
        models.clone(),
        Arc::new(ScopedAuthSpy::default()),
        principal(false),
    );
    for (owner, binding, expected) in [
        ("other", Some(&reader), "unauthorized"),
        ("owner", None, "unsupported"),
    ] {
        let result = handle_model_catalog(&json!({}), owner, binding).await;
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(value["error"]["error_kind"], expected);
    }
    assert_eq!(models.reads.load(Ordering::SeqCst), 0);
    for (status, expected, retryable) in [
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable", true),
        (StatusCode::UNAUTHORIZED, "unauthorized", false),
        (StatusCode::BAD_REQUEST, "invalid_catalog", false),
        (StatusCode::NOT_IMPLEMENTED, "unsupported", false),
    ] {
        *models.failure.lock().unwrap() = Some(status);
        let result = handle_model_catalog(&json!({}), "owner", Some(&reader)).await;
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert!(result.is_error);
        assert_eq!(value["error"]["error_kind"], expected);
        assert_eq!(value["error"]["retryable"], retryable);
        assert!(value["total"].is_null());
        assert!(!result.output.contains("private"));
    }
}

#[tokio::test]
async fn public_executor_pages_fresh_catalog_and_detects_change() {
    let models = Arc::new(CatalogSpy::default());
    *models.items.lock().unwrap() = (0..20).map(fixture).collect();
    let reader = AuthorizedModelCatalogReader::new(
        models.clone(),
        Arc::new(ScopedAuthSpy::default()),
        principal(false),
    );
    let workspace = tempfile::tempdir().unwrap();
    let executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
        workspace.path().into(),
        "owner".into(),
        "session".into(),
        None,
        None,
    )
    .with_model_catalog_reader(Some(reader));
    assert_eq!(models.reads.load(Ordering::SeqCst), 0);
    let ordinary = executor
        .execute_with_metadata("introspect", &json!({"facet":"errors"}))
        .await;
    assert!(!ordinary.is_error);
    for args in [
        json!({"user_id":"other"}),
        json!({"artifact":"x"}),
        json!({"depth":"forensic"}),
        json!({"source_policy":"cloud_only"}),
        json!({"question":42}),
        json!({"catalog":{}}),
    ] {
        assert!(
            executor
                .execute_with_metadata("model_catalog", &args)
                .await
                .is_error
        );
    }
    assert_eq!(models.reads.load(Ordering::SeqCst), 0);
    let first = executor
        .execute_with_metadata("model_catalog", &json!({"limit":16}))
        .await;
    assert!(!first.is_error, "{}", first.output);
    let canonical: astra_turn_core::model_catalog::ModelCatalogPage =
        serde_json::from_str(&first.output).unwrap();
    assert_eq!(
        first.output,
        canonical.to_json(),
        "recording requires canonical JSON bytes"
    );
    assert_eq!(models.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        astra_tools::model_result_presentation(first.metadata.as_ref()),
        astra_tools::ModelResultPresentation::SourceBounded
    );
    let value: Value = serde_json::from_str(&first.output).unwrap();
    assert_eq!(value["returned"], 16);
    assert!(
        first.output.len() > 4_000,
        "exercise model catalog presentation boundary"
    );
    let args = json!({
        "cursor":value["next_cursor"],"catalog_revision":value["catalog_revision"]
    });
    let next = executor.execute_with_metadata("model_catalog", &args).await;
    assert!(!next.is_error);
    let value: Value = serde_json::from_str(&next.output).unwrap();
    assert_eq!(value["returned"], 4);
    assert_eq!(value["items"][0]["offering_id"], "id-016");
    assert_eq!(models.reads.load(Ordering::SeqCst), 2);
    models.items.lock().unwrap()[0].is_active = false;
    let changed = executor.execute_with_metadata("model_catalog", &args).await;
    let value: Value = serde_json::from_str(&changed.output).unwrap();
    assert!(changed.is_error);
    assert_eq!(value["error"]["error_kind"], "catalog_changed");
    assert_eq!(models.reads.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn admission_snapshot_is_shared_across_reader_clones() {
    let models = Arc::new(CatalogSpy::default());
    *models.items.lock().unwrap() = vec![fixture(0)];
    let reader = AuthorizedModelCatalogReader::new(
        models.clone(),
        Arc::new(ScopedAuthSpy::default()),
        principal(false),
    );
    let child = reader.clone();
    assert_eq!(reader.read_snapshot().await.unwrap().items.len(), 1);
    assert_eq!(child.read_snapshot().await.unwrap().items.len(), 1);
    assert_eq!(models.reads.load(Ordering::SeqCst), 1);
}
