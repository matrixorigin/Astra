//! Server-owned Genesis model access. UC subjects are resolved through the
//! canonical external-identity mapping; no model credential enters the CLI.
use super::*;
use crate::auth::uc::UcNativeProvider;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

#[derive(Deserialize)]
struct GenesisModel {
    id: String,
    name: String,
    #[serde(rename = "type")]
    model_type: String,
    status: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    context_window: i32,
    #[serde(default)]
    max_output_tokens: i32,
}

#[derive(Deserialize)]
struct GenesisPage {
    items: Vec<GenesisModel>,
    next_page_token: String,
}

pub(super) struct GenesisCatalog {
    pub items: Vec<ModelListItem>,
    pub default_offering_id: Option<String>,
    api_key: String,
}

#[derive(Deserialize)]
struct MoiChatModelPolicy {
    default_model: String,
    models: Vec<String>,
}

fn policy_unavailable() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "MOI agent chat model policy is not ready",
        "moi_model_policy_unavailable",
    )
}

fn unavailable() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "Genesis model access is not ready",
        "genesis_not_ready",
    )
}

pub(super) fn offering_id(issuer: &str, subject: &str, id: &str) -> String {
    let mut hash = Sha256::new();
    for part in [issuer, subject, id] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    // Full SHA-256 in base64url fits Astra's 64-byte Offering identity limit.
    format!("genesis-{}", URL_SAFE_NO_PAD.encode(hash.finalize()))
}

impl DatabaseModelService {
    pub fn with_uc_native(mut self, provider: Option<UcNativeProvider>) -> Self {
        self.uc_provider = provider;
        self
    }

    pub(super) async fn uc_subject(
        &self,
        user_id: &str,
    ) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
        let Some(provider) = &self.uc_provider else {
            return Ok(None);
        };
        let pool = self.get_pool().await.map_err(internal_error)?;
        query_scalar("SELECT external_subject FROM auth_external_identities WHERE astra_user_id = ? AND provider_id = ? LIMIT 1")
            .bind(user_id).bind(format!("uc:{}", provider.settings.issuer))
            .fetch_optional(&pool).await.map_err(internal_error)
    }

    pub(super) async fn genesis_catalog(
        &self,
        subject: &str,
    ) -> Result<GenesisCatalog, (StatusCode, Json<ErrorResponse>)> {
        let provider = self.uc_provider.as_ref().ok_or_else(unavailable)?;
        let mut url = crate::auth::uc::validate_uc_url(&provider.settings.adapter_url)
            .map_err(|_| unavailable())?;
        url.path_segments_mut().map_err(|_| unavailable())?.extend([
            "api",
            "v1",
            "uc",
            "internal",
            "accounts",
            subject,
            "aistudio-pat",
        ]);
        #[derive(Deserialize)]
        struct RuntimePAT {
            api_key: String,
        }
        let response = provider
            .client
            .get(url)
            .bearer_auth(provider.service_bearer().await?)
            .send()
            .await
            .map_err(|_| unavailable())?;
        let pat: RuntimePAT = UcNativeProvider::json(response)
            .await
            .map_err(|_| unavailable())?;
        if pat.api_key.is_empty() {
            return Err(unavailable());
        }
        // MOI owns product selection; Genesis owns account eligibility. The
        // runtime PAT remains server-side and uses MOI's existing PAT transport.
        // No workspace is needed to read this account-level product policy.
        #[derive(Deserialize)]
        struct PolicyEnvelope {
            code: String,
            data: MoiChatModelPolicy,
        }
        let response = provider
            .client
            .get(format!(
                "{}/aistudio/model-policy",
                provider.settings.moi_api_url
            ))
            .header("X-API-Key", &pat.api_key)
            .send()
            .await
            .map_err(|_| policy_unavailable())?;
        let envelope: PolicyEnvelope = UcNativeProvider::json_bounded(response, 1024 * 1024)
            .await
            .map_err(|_| policy_unavailable())?;
        let policy = envelope.data;
        let allowed: BTreeSet<_> = policy.models.iter().cloned().collect();
        if envelope.code != "OK"
            || allowed.len() != policy.models.len()
            || allowed.contains("")
            || !allowed.contains(&policy.default_model)
        {
            return Err(policy_unavailable());
        }
        let mut cursor = String::new();
        let mut seen = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut items = Vec::new();
        // Genesis owns visibility and status; use its existing user-PAT API,
        // including pagination, rather than a second local model allowlist.
        for _ in 0..100 {
            let response = provider
                .client
                .get(format!(
                    "{}/api/v1/taas/llm/model-offerings",
                    provider.settings.genesis_url
                ))
                .bearer_auth(&pat.api_key)
                .query(&[
                    ("type", "chat"),
                    ("status", "enabled"),
                    ("page_size", "100"),
                    ("page_token", cursor.as_str()),
                ])
                .send()
                .await
                .map_err(|_| unavailable())?;
            let page: GenesisPage = UcNativeProvider::json_bounded(response, 1024 * 1024)
                .await
                .map_err(|_| unavailable())?;
            for model in page.items {
                if !matches!(model.model_type.as_str(), "chat_text" | "chat_multimodal")
                    || model.status != "enabled"
                {
                    return Err(unavailable());
                }
                if model.id.is_empty()
                    || model.name.is_empty()
                    || model.context_window <= 0
                    || !ids.insert(model.id.clone())
                {
                    return Err(unavailable());
                }
                items.push(ModelListItem {
                    offering_id: offering_id(&provider.settings.issuer, subject, &model.id),
                    access_id: "genesis".into(),
                    access_kind: ModelAccessKind::AstraCloud,
                    access_label: "Genesis".into(),
                    execution_placement: ModelExecutionPlacement::Server,
                    name: model.name,
                    provider: "openai".into(),
                    description: Some(model.description),
                    is_active: true,
                    context_window: model.context_window,
                    max_completion_tokens: (model.max_output_tokens > 0)
                        .then_some(model.max_output_tokens),
                    architecture: None,
                    thinking_capability: None,
                });
            }
            if page.next_page_token.is_empty() {
                items.retain(|item| allowed.contains(&item.name));
                let default_offering_id = items
                    .iter()
                    .find(|item| item.name == policy.default_model)
                    .map(|item| item.offering_id.clone());
                sort_model_list_items(&mut items);
                return Ok(GenesisCatalog {
                    items,
                    default_offering_id,
                    api_key: pat.api_key,
                });
            }
            if !seen.insert(page.next_page_token.clone()) {
                return Err(unavailable());
            }
            cursor = page.next_page_token;
        }
        Err(unavailable())
    }

    pub(super) async fn admit_genesis(
        &self,
        subject: &str,
        selected: &str,
    ) -> Result<AdmittedModelExecution, (StatusCode, Json<ErrorResponse>)> {
        let catalog = self.genesis_catalog(subject).await?;
        let item = catalog
            .items
            .into_iter()
            .find(|item| item.offering_id == selected)
            .ok_or_else(|| {
                model_offering_resolution_error_response(ModelOfferingResolutionError::NotFound {
                    offering_id: selected.into(),
                })
            })?;
        let provider = self.uc_provider.as_ref().ok_or_else(unavailable)?;
        Ok(AdmittedModelExecution {
            offering_id: item.offering_id,
            access_kind: ModelAccessKind::AstraCloud,
            execution_placement: ModelExecutionPlacement::Server,
            model_name: item.name,
            wire_model_name: None,
            api_key: catalog.api_key,
            base_url: format!("{}/v1", provider.settings.genesis_url),
            provider: "openai".into(),
            cache_capability: None,
            thinking_capability: None,
            fixed_temperature: None,
            thinking_protocol: None,
            request_body_overrides: None,
            context_window: Some(item.context_window as u32),
            max_completion_tokens: item.max_completion_tokens.map(|v| v as u32),
            header_overrides: HashMap::new(),
            completions_url_override: None,
            request_timeout_ms: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::config::UcNativeSettings;
    use axum::{
        Router,
        extract::{Path, Query, State},
        http::HeaderMap,
        routing::{get, post},
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone, Default)]
    struct Fixture {
        requests: Arc<AtomicUsize>,
        unavailable: Arc<Mutex<Option<&'static str>>>,
        policy: Arc<Mutex<serde_json::Value>>,
        policy_status: Arc<Mutex<StatusCode>>,
    }

    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn fixture() -> (DatabaseModelService, Fixture, Server) {
        let state = Fixture {
            policy: Arc::new(Mutex::new(serde_json::json!({
                "code": "OK",
                "data": {"default_model": "genesis-model-1", "models": ["genesis-model-1", "genesis-model-2"]}
            }))),
            ..Fixture::default()
        };
        let app = Router::new()
            .route("/realms/moi/protocol/openid-connect/token", post(|headers: HeaderMap| async move {
                assert!(headers["authorization"].to_str().unwrap().starts_with("Basic "));
                Json(serde_json::json!({"access_token":"synthetic-service", "token_type":"Bearer", "expires_in":300}))
            }))
            .route("/api/v1/uc/internal/accounts/{subject}/aistudio-pat", get(|Path(subject): Path<String>, headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-service");
                assert!(!headers.contains_key("x-api-key"));
                Json(serde_json::json!({"api_key":format!("synthetic-pat-{subject}")}))
            }))
            .route("/newmoi/aistudio/model-policy", get(|State(state): State<Fixture>, headers: HeaderMap| async move {
                assert!(headers["x-api-key"].to_str().unwrap().starts_with("synthetic-pat-"));
                assert!(!headers.contains_key("authorization"));
                assert!(!headers.contains_key("x-workspace-id"));
                (*state.policy_status.lock().unwrap(), Json(state.policy.lock().unwrap().clone()))
            }))
            .route("/api/v1/taas/llm/model-offerings", get(|State(state): State<Fixture>, headers: HeaderMap, Query(query): Query<HashMap<String,String>>| async move {
                state.requests.fetch_add(1, Ordering::SeqCst);
                assert!(!headers.contains_key("x-api-key"));
                assert!(headers["authorization"].to_str().unwrap().starts_with("Bearer synthetic-pat-"));
                assert_eq!(query.get("type").unwrap(), "chat");
                assert_eq!(query.get("status").unwrap(), "enabled");
                let failure = *state.unavailable.lock().unwrap();
                let first = query.get("page_token").unwrap().is_empty();
                let (id, cursor) = if first { ("model-1", "next") } else { ("model-2", "") };
                let item = serde_json::json!({"id":id,"name":format!("genesis-{id}"),"type": if first {"chat_text"} else {"chat_multimodal"}, "status":if failure == Some("disabled") {"disabled"} else {"enabled"}, "context_window":if failure == Some("context") {0} else {32000}, "max_output_tokens":4096});
                let items = if failure == Some("removed") { vec![] } else { vec![item] };
                Json(serde_json::json!({"items":items, "next_page_token": if failure == Some("cycle") { "next" } else { cursor }}))
            })).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let provider = UcNativeProvider::new(UcNativeSettings {
            builtin_memory: false,
            issuer: format!("{origin}/realms/moi"),
            adapter_url: origin.clone(),
            moi_api_url: format!("{origin}/newmoi"),
            genesis_url: origin,
            client_secret: "synthetic-service-secret".into(),
        })
        .unwrap();
        let service = DatabaseModelService::new(
            MatrixOneSettings::default(),
            Arc::new(FernetTokenEncryptor::new("synthetic-test-key").unwrap()),
        )
        .with_uc_native(Some(provider));
        (service, state, server)
    }

    #[tokio::test]
    async fn genesis_catalog_is_paginated_account_bound_and_never_exports_credentials() {
        let (service, state, _server) = fixture().await;
        let a = service.genesis_catalog("account-A").await.unwrap();
        let b = service.genesis_catalog("account-B").await.unwrap();
        assert_eq!(a.items.len(), 2);
        assert_eq!(state.requests.load(Ordering::SeqCst), 4);
        assert_ne!(a.items[0].offering_id, b.items[0].offering_id);
        for item in &a.items {
            validate_model_offering_id(&item.offering_id).unwrap();
        }
        let projection = project_model_access_with_default(
            server_model_access_declarations(false, a.items.iter().map(|item| item.access_kind)),
            a.items
                .iter()
                .cloned()
                .map(ModelListItemResponse::from)
                .collect(),
            Some(ModelDefaultCandidate {
                offering_id: a.items[0].offering_id.clone(),
                source: ModelDefaultSource::Astra,
                scope: ModelDefaultScope::EffectiveCatalog,
            }),
            "2026-09-16T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(
            projection.default_offering_id.as_ref(),
            Some(&a.items[0].offering_id)
        );
        let exported = serde_json::to_string(&a.items).unwrap();
        assert!(!exported.contains("synthetic-") && !exported.contains("api_key"));
        let admitted = service
            .admit_genesis("account-A", &a.items[0].offering_id)
            .await
            .unwrap();
        assert_eq!(admitted.api_key, "synthetic-pat-account-A");
        assert_eq!(admitted.access_kind, ModelAccessKind::AstraCloud);
        assert_eq!(
            admitted.execution_placement,
            ModelExecutionPlacement::Server
        );
        assert!(admitted.base_url.ends_with("/v1"));
        assert!(
            service
                .admit_genesis("account-B", &a.items[0].offering_id)
                .await
                .is_err()
        );
        *state.unavailable.lock().unwrap() = Some("removed");
        assert!(
            service
                .admit_genesis("account-A", &a.items[0].offering_id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn genesis_catalog_rejects_disabled_invalid_and_cyclic_metadata() {
        let (service, state, _server) = fixture().await;
        for failure in ["disabled", "context", "cycle"] {
            *state.unavailable.lock().unwrap() = Some(failure);
            assert!(
                service.genesis_catalog("account-A").await.is_err(),
                "{failure}"
            );
        }
    }

    #[tokio::test]
    async fn moi_policy_controls_catalog_default_and_live_admission() {
        let (service, state, _server) = fixture().await;
        let initial = service.genesis_catalog("account-A").await.unwrap();
        assert_eq!(
            initial.default_offering_id,
            Some(initial.items[0].offering_id.clone())
        );
        let removed = initial.items[0].offering_id.clone();
        *state.policy.lock().unwrap() = serde_json::json!({
            "code": "OK", "data": {"models": ["genesis-model-2"], "default_model": "genesis-model-2"}
        });
        let changed = service.genesis_catalog("account-A").await.unwrap();
        assert_eq!(changed.items.len(), 1);
        assert_eq!(changed.items[0].name, "genesis-model-2");
        assert_eq!(
            changed.default_offering_id,
            Some(changed.items[0].offering_id.clone())
        );
        assert!(service.admit_genesis("account-A", &removed).await.is_err());
        assert!(
            service
                .admit_genesis("account-A", &changed.items[0].offering_id)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn moi_default_is_explicit_not_catalog_sort_order() {
        let (service, state, _server) = fixture().await;
        state.policy.lock().unwrap()["data"]["default_model"] = "genesis-model-2".into();
        let catalog = service.genesis_catalog("account-A").await.unwrap();
        assert_eq!(catalog.items.len(), 2);
        assert_eq!(
            catalog.default_offering_id,
            Some(catalog.items[1].offering_id.clone())
        );
    }

    #[tokio::test]
    async fn moi_policy_cannot_expand_genesis_entitlements_or_invent_a_default() {
        let (service, state, _server) = fixture().await;
        *state.policy.lock().unwrap() = serde_json::json!({
            "code": "OK", "data": {"models": ["genesis-model-2", "not-entitled"], "default_model": "not-entitled"}
        });
        let catalog = service.genesis_catalog("account-A").await.unwrap();
        assert_eq!(catalog.items.len(), 1);
        assert_eq!(catalog.items[0].name, "genesis-model-2");
        assert_eq!(catalog.default_offering_id, None);
        *state.unavailable.lock().unwrap() = Some("removed");
        let empty = service.genesis_catalog("account-A").await.unwrap();
        assert!(empty.items.is_empty());
        assert_eq!(empty.default_offering_id, None);
    }

    #[tokio::test]
    async fn moi_policy_failure_never_falls_back_to_the_full_genesis_catalog() {
        let (service, state, _server) = fixture().await;
        let selected = service.genesis_catalog("account-A").await.unwrap().items[0]
            .offering_id
            .clone();
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            *state.policy_status.lock().unwrap() = status;
            let error = service
                .admit_genesis("account-A", &selected)
                .await
                .err()
                .unwrap();
            assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                serde_json::to_string(&error.1.0)
                    .unwrap()
                    .contains("moi_model_policy_unavailable")
            );
        }
        *state.policy_status.lock().unwrap() = StatusCode::OK;
        for policy in [
            serde_json::json!({"code": "ErrLLMServiceSlotNotConfigured", "data": null}),
            serde_json::json!({"code": "OK", "data": {"models": [], "default_model": ""}}),
            serde_json::json!({"code": "OK", "data": {"models": ["genesis-model-1"], "default_model": "outside"}}),
            serde_json::json!({"code": "OK", "data": {"models": [""], "default_model": ""}}),
            serde_json::json!({"code": "OK", "data": {"models": ["genesis-model-1"]}}),
            serde_json::json!({"code": "OK", "data": {"models": ["genesis-model-1", "genesis-model-1"], "default_model": "genesis-model-1"}}),
        ] {
            *state.policy.lock().unwrap() = policy;
            assert!(service.admit_genesis("account-A", &selected).await.is_err());
        }
    }
}
