use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use crate::turn::agentic_loop::host::AgenticLoopHost;
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
    pause: Mutex<Option<Arc<tokio::sync::Notify>>>,
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
    async fn list_models(&self, user: String, _: bool) -> HttpResult<Vec<ModelListItem>> {
        assert_eq!(user, "owner");
        tokio::task::yield_now().await;
        self.reads.fetch_add(1, Ordering::SeqCst);
        let pause = self.pause.lock().unwrap().clone();
        if let Some(pause) = pause {
            pause.notified().await;
        }
        if let Some(status) = *self.failure.lock().unwrap() {
            return Err(error_response(
                status,
                "private backend endpoint and credential must not escape",
            ));
        }
        Ok(self.items.lock().unwrap().clone())
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
        assert!(principal.is_provider_authorized_request());
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
        execution_continuation: None,
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

fn runtime_provider_principal() -> AuthPrincipal {
    let mut principal = principal(true);
    if let AuthPrincipalOrigin::ProviderAuthorizedRequest(context) = &mut principal.origin {
        context.edge_agent_id = None;
    }
    principal
}

fn fixture(index: usize) -> ModelListItem {
    ModelListItem {
        thinking_protocol: None,
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

#[tokio::test(start_paused = true)]
async fn catalog_observation_cache_coalesces_and_isolates_generations() {
    let models = Arc::new(CatalogSpy::default());
    *models.items.lock().unwrap() = vec![fixture(0)];
    let auth = Arc::new(ScopedAuthSpy::default());
    let cache = AuthorizedModelCatalogCache::default();
    let bind = |principal| {
        AuthorizedModelCatalogReader::with_cache(
            models.clone(),
            auth.clone(),
            principal,
            cache.clone(),
        )
    };
    let first = bind(principal(false));
    let second = bind(principal(false));
    let (a, b) = tokio::join!(first.read_snapshot(), second.read_snapshot());
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(models.reads.load(Ordering::SeqCst), 1);
    *models.items.lock().unwrap() = vec![fixture(1)];
    assert_eq!(
        bind(principal(false)).read_snapshot().await.unwrap()[0].offering_id,
        "id-000"
    );
    assert_eq!(models.reads.load(Ordering::SeqCst), 1);
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    assert_eq!(
        bind(principal(false)).read_snapshot().await.unwrap()[0].offering_id,
        "id-001"
    );
    assert_eq!(models.reads.load(Ordering::SeqCst), 2);
    assert_eq!(
        first.read_snapshot().await.unwrap()[0].offering_id,
        "id-000",
        "running requests retain their observed generation"
    );
    let mut other_session = principal(false);
    other_session.session_id = Some("another-auth-session".into());
    bind(other_session).read_snapshot().await.unwrap();
    assert_eq!(models.reads.load(Ordering::SeqCst), 3);
    bind(principal(true)).read_snapshot().await.unwrap();
    assert_eq!(auth.reads.load(Ordering::SeqCst), 1);
    let mut other_authorization = principal(true);
    if let AuthPrincipalOrigin::ProviderAuthorizedRequest(context) = &mut other_authorization.origin
    {
        context.request_authorization_id = "different-authorization".into();
    }
    bind(other_authorization).read_snapshot().await.unwrap();
    assert_eq!(auth.reads.load(Ordering::SeqCst), 2);
    assert_eq!(
        models.reads.load(Ordering::SeqCst),
        3,
        "provider scopes cannot widen to the owner catalog"
    );
}

#[tokio::test]
async fn catalog_cache_failure_is_request_local_and_refresh_observes_revocation() {
    let models = Arc::new(CatalogSpy::default());
    let auth = Arc::new(ScopedAuthSpy::default());
    let cache = AuthorizedModelCatalogCache::default();
    let bind = || {
        AuthorizedModelCatalogReader::with_cache(
            models.clone(),
            auth.clone(),
            principal(false),
            cache.clone(),
        )
    };
    *models.failure.lock().unwrap() = Some(StatusCode::SERVICE_UNAVAILABLE);
    let failed = bind();
    assert!(failed.read_snapshot().await.is_err());
    assert!(failed.read_snapshot().await.is_err());
    assert_eq!(models.reads.load(Ordering::SeqCst), 1);
    *models.failure.lock().unwrap() = None;
    *models.items.lock().unwrap() = vec![fixture(0)];
    let recovered = bind();
    recovered.read_snapshot().await.unwrap();
    assert_eq!(models.reads.load(Ordering::SeqCst), 2);
    *models.failure.lock().unwrap() = Some(StatusCode::FORBIDDEN);
    assert!(recovered.read_fresh_items().await.is_err());
    assert!(
        bind().read_snapshot().await.is_err(),
        "failed refresh invalidates shared observation"
    );
    assert_eq!(models.reads.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn catalog_context_projects_safe_stable_candidates_and_partial_coverage() {
    let models = Arc::new(CatalogSpy::default());
    *models.items.lock().unwrap() = vec![fixture(0)];
    let reader = AuthorizedModelCatalogReader::new(
        models.clone(),
        Arc::new(ScopedAuthSpy::default()),
        principal(false),
    );
    let host = super::super::server_loop_host::ServerAgenticLoopHostBuilder::new(
        crate::MatrixOneSettings::default(),
        Arc::new(crate::FernetTokenEncryptor::new("catalog-test-only").unwrap()),
        "owner".into(),
        "session".into(),
    )
    .with_model_catalog_reader(Some(reader.clone()))
    .build();
    let before_tools = host.valid_tool_names();
    let first = host.model_catalog_context().await.unwrap();
    assert_eq!(first, host.model_catalog_context().await.unwrap());
    assert_eq!(models.reads.load(Ordering::SeqCst), 1);
    assert!(crate::turn::wire_assembly::is_required_runtime_preamble(
        &first
    ));
    let content = first["content"].as_str().unwrap();
    let projected: Value = serde_json::from_str(content).unwrap();
    assert_eq!(
        projected.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["catalog", "child_model_selection_allowed"]
    );
    assert_eq!(projected["catalog"]["coverage"], "complete");
    assert_eq!(projected["child_model_selection_allowed"], true);
    assert_eq!(projected["catalog"]["items"][0]["offering_id"], "id-000");
    assert!(projected["catalog"]["observed_at"].is_null());
    assert!(!content.contains("private"));
    assert_eq!(
        before_tools,
        host.valid_tool_names(),
        "observing candidates never mutates tool authority or stable schemas"
    );
    *models.items.lock().unwrap() = (0..20).map(fixture).collect();
    reader.read_fresh_items().await.unwrap();
    let partial = host.model_catalog_context().await.unwrap();
    let projected: Value = serde_json::from_str(partial["content"].as_str().unwrap()).unwrap();
    assert_eq!(projected["catalog"]["coverage"], "page");
    assert_eq!(projected["catalog"]["returned"], 16);
    assert!(projected["catalog"]["next_cursor"].is_string());
    assert_eq!(models.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn catalog_context_keeps_provider_child_selection_ceiling() {
    let models = Arc::new(CatalogSpy::default());
    let auth = Arc::new(ScopedAuthSpy::default());
    for principal in [principal(true), runtime_provider_principal()] {
        let reader = AuthorizedModelCatalogReader::new(models.clone(), auth.clone(), principal);
        let host = super::super::server_loop_host::ServerAgenticLoopHostBuilder::new(
            crate::MatrixOneSettings::default(),
            Arc::new(crate::FernetTokenEncryptor::new("catalog-test-only").unwrap()),
            "owner".into(),
            "session".into(),
        )
        .with_model_catalog_reader(Some(reader))
        .build();
        let context = host.model_catalog_context().await.unwrap();
        let projected: Value = serde_json::from_str(context["content"].as_str().unwrap()).unwrap();
        assert_eq!(projected["catalog"]["coverage"], "complete");
        assert_eq!(projected["child_model_selection_allowed"], false);
        assert!(!context.to_string().contains("private"));
    }
    assert_eq!(models.reads.load(Ordering::SeqCst), 0);
    assert_eq!(auth.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn catalog_observation_cache_does_not_retain_oversized_catalogs() {
    let models = Arc::new(CatalogSpy::default());
    let mut item = fixture(0);
    item.description = Some("x".repeat(65_536));
    *models.items.lock().unwrap() = vec![item];
    let auth = Arc::new(ScopedAuthSpy::default());
    let cache = AuthorizedModelCatalogCache::default();
    for _ in 0..2 {
        let reader = AuthorizedModelCatalogReader::with_cache(
            models.clone(),
            auth.clone(),
            principal(false),
            cache.clone(),
        );
        reader.read_snapshot().await.unwrap();
        reader.read_snapshot().await.unwrap();
    }
    assert_eq!(
        models.reads.load(Ordering::SeqCst),
        2,
        "large snapshots are retained only in the request, not across requests"
    );
}

#[tokio::test]
async fn catalog_observation_cache_evicts_principals_at_capacity() {
    let models = Arc::new(CatalogSpy::default());
    *models.items.lock().unwrap() = vec![fixture(0)];
    let auth = Arc::new(ScopedAuthSpy::default());
    let cache = AuthorizedModelCatalogCache::default();
    let bind = |session: usize| {
        let mut principal = principal(false);
        principal.session_id = Some(format!("auth-session-{session}"));
        AuthorizedModelCatalogReader::with_cache(
            models.clone(),
            auth.clone(),
            principal,
            cache.clone(),
        )
    };
    for session in 0..1025 {
        bind(session).read_snapshot().await.unwrap();
    }
    assert_eq!(models.reads.load(Ordering::SeqCst), 1025);
    bind(1024).read_snapshot().await.unwrap();
    assert_eq!(models.reads.load(Ordering::SeqCst), 1025);
    bind(0).read_snapshot().await.unwrap();
    assert_eq!(models.reads.load(Ordering::SeqCst), 1026);
}

#[tokio::test(start_paused = true)]
async fn catalog_deadline_includes_waiters_and_cancelled_refresh_invalidates_success() {
    let models = Arc::new(CatalogSpy::default());
    *models.items.lock().unwrap() = vec![fixture(0)];
    *models.pause.lock().unwrap() = Some(Arc::new(tokio::sync::Notify::new()));
    let auth = Arc::new(ScopedAuthSpy::default());
    let cache = AuthorizedModelCatalogCache::default();
    let bind = || {
        AuthorizedModelCatalogReader::with_cache(
            models.clone(),
            auth.clone(),
            principal(false),
            cache.clone(),
        )
    };
    let first = bind();
    let a = tokio::spawn({
        let reader = first.clone();
        async move { reader.read_snapshot().await }
    });
    let b = tokio::spawn({
        let reader = bind();
        async move { reader.read_snapshot().await }
    });
    while models.reads.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    assert!(a.await.unwrap().is_err());
    assert!(b.await.unwrap().is_err());
    assert_eq!(
        models.reads.load(Ordering::SeqCst),
        1,
        "waiters share the total deadline, not serial backend timeouts"
    );
    assert!(first.read_snapshot().await.is_err());
    assert_eq!(models.reads.load(Ordering::SeqCst), 1);
    *models.pause.lock().unwrap() = None;
    let recovered = bind();
    recovered.read_snapshot().await.unwrap();
    assert_eq!(models.reads.load(Ordering::SeqCst), 2);
    *models.pause.lock().unwrap() = Some(Arc::new(tokio::sync::Notify::new()));
    let refreshing = tokio::spawn({
        let reader = recovered.clone();
        async move { reader.read_fresh_items().await }
    });
    while models.reads.load(Ordering::SeqCst) != 3 {
        tokio::task::yield_now().await;
    }
    refreshing.abort();
    assert!(refreshing.await.unwrap_err().is_cancelled());
    assert!(recovered.cached_snapshot().await.is_none());
    assert!(recovered.read_snapshot().await.is_err());
    *models.pause.lock().unwrap() = None;
    *models.items.lock().unwrap() = vec![fixture(1)];
    assert_eq!(
        bind().read_snapshot().await.unwrap()[0].offering_id,
        "id-001",
        "cancelled refresh cannot leave shared stale success"
    );
    assert_eq!(models.reads.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn provider_scope_never_reads_owner_catalog() {
    let models = Arc::new(CatalogSpy::default());
    let auth = Arc::new(ScopedAuthSpy::default());
    let reader = AuthorizedModelCatalogReader::new(
        models.clone(),
        auth.clone(),
        runtime_provider_principal(),
    );
    let result = handle_model_catalog(&json!({}), "owner", Some(&reader)).await;
    let value: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(value["principal_scope"], "provider_scope");
    assert_eq!(value["items"][0]["offering_id"], "scoped-offering");
    assert_eq!(models.reads.load(Ordering::SeqCst), 0);
    assert_eq!(auth.reads.load(Ordering::SeqCst), 1);
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
    .with_model_catalog_reader(Some(reader.clone()));
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
        json!({"cursor":"next"}),
        json!({"catalog_revision":format!("sha256:{}", "0".repeat(64))}),
    ] {
        assert!(
            executor
                .execute_with_metadata("model_catalog", &args)
                .await
                .is_error
        );
    }
    assert_eq!(models.reads.load(Ordering::SeqCst), 0);
    let malformed = handle_model_catalog(&json!({"cursor":"next"}), "owner", Some(&reader)).await;
    let evidence: astra_core::ToolFailureEvidence =
        serde_json::from_value(malformed.metadata.as_ref().unwrap()["recovery_evidence"].clone())
            .unwrap();
    assert_eq!(
        evidence.cause,
        astra_core::ToolFailureCause::InvalidArguments
    );
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
    let refreshed = reader.read_snapshot().await.unwrap();
    assert!(
        !refreshed[0].is_active,
        "fresh discovery replaces the lookup snapshot"
    );
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
    let (first, second) = tokio::join!(reader.read_snapshot(), child.read_snapshot());
    assert_eq!(first.unwrap().len(), 1);
    assert_eq!(second.unwrap().len(), 1);
    assert_eq!(models.reads.load(Ordering::SeqCst), 1);
}
