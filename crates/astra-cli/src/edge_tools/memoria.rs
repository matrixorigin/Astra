//! Memoria (memory service) integration for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use super::ToolExecutor;

fn current_memoria_proxy_target() -> Result<(String, String), String> {
    let base = crate::cli::config_manager::resolve_api_url(None)?;
    let token =
        crate::cli::session::session_runtime::current_access_token(None).ok_or_else(|| {
            "not logged in; memory operations must go through the Astra server".to_string()
        })?;
    Ok((base, token))
}

async fn memoria_proxy_request(
    method: astra_tools::memoria::HttpMethod,
    path: &str,
    timeout: Duration,
    body: Option<&Value>,
) -> Result<String, String> {
    // Capture destination and credential authority together before awaiting a
    // refresh. A workspace .env must not redirect the native account's bearer,
    // and an in-flight request must not adopt a later login generation.
    let binding = crate::cli::native_auth::active();
    let (base, token) = if let Some(binding) = &binding {
        (binding.endpoint().to_owned(), binding.access_token().await?)
    } else {
        current_memoria_proxy_target()?
    };
    let builder = astra_core::net::client_builder_for_target(&base).timeout(timeout);
    let builder = if binding.is_some() {
        builder.redirect(reqwest::redirect::Policy::none())
    } else {
        builder
    };
    let client = builder.build().map_err(|e| format!("build client: {e}"))?;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let req = match method {
        astra_tools::memoria::HttpMethod::Get => client.get(&url),
        astra_tools::memoria::HttpMethod::Put => client.put(&url),
        astra_tools::memoria::HttpMethod::Post => client.post(&url),
    }
    .header("Authorization", format!("Bearer {token}"));
    let req = if let Some(body) = body {
        req.json(body)
    } else {
        req
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(text)
    } else {
        Err(format!("({status}) {text}"))
    }
}

pub async fn memoria_snapshot_create(name: &str) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        "/memory/snapshots",
        Duration::from_secs(5),
        Some(&json!({ "name": name })),
    )
    .await
}

pub async fn memoria_snapshot_rollback(name: &str) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        &format!("/memory/snapshots/{name}/rollback"),
        Duration::from_secs(10),
        None,
    )
    .await
}

pub async fn memoria_snapshot_diff(name: &str) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Get,
        &format!("/memory/snapshots/{name}/diff"),
        Duration::from_secs(5),
        None,
    )
    .await
}

pub async fn memoria_snapshots_list() -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Get,
        "/memory/snapshots",
        Duration::from_secs(5),
        None,
    )
    .await
}

pub async fn memoria_branch_create(name: &str) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        "/memory/branches",
        Duration::from_secs(5),
        Some(&json!({ "name": name })),
    )
    .await
}

pub async fn memoria_branch_checkout(name: &str) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        &format!("/memory/branches/{name}/checkout"),
        Duration::from_secs(5),
        None,
    )
    .await
}

pub async fn memoria_branch_merge(name: &str) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        &format!("/memory/branches/{name}/merge"),
        Duration::from_secs(5),
        None,
    )
    .await
}

pub async fn memoria_branch_diff(name: &str) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Get,
        &format!("/memory/branches/{name}/diff"),
        Duration::from_secs(5),
        None,
    )
    .await
}

pub async fn memoria_branches_list() -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Get,
        "/memory/branches",
        Duration::from_secs(5),
        None,
    )
    .await
}

pub async fn memoria_reflect() -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        "/memory/reflect",
        Duration::from_secs(15),
        Some(&json!({ "mode": "auto" })),
    )
    .await
}

pub async fn memoria_health() -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Get,
        "/memory/health",
        Duration::from_secs(5),
        None,
    )
    .await
}

pub async fn memoria_feedback(
    memory_id: &str,
    signal: &str,
    context: Option<&str>,
) -> Result<String, String> {
    let mut body = json!({ "signal": signal });
    if let Some(context) = context {
        body["context"] = json!(context);
    }
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        &format!("/memory/feedback/{memory_id}"),
        Duration::from_secs(5),
        Some(&body),
    )
    .await
}

pub async fn memoria_show(memory_id: &str) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Get,
        &format!("/memory/expand/{memory_id}"),
        Duration::from_secs(5),
        None,
    )
    .await
}

pub async fn memoria_purge(body: &Value) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        "/memory/purge",
        Duration::from_secs(5),
        Some(body),
    )
    .await
}

pub async fn memoria_retrieve(body: &Value, timeout: Duration) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        "/memory/retrieve",
        timeout,
        Some(body),
    )
    .await
}

pub async fn memoria_store(body: &Value, timeout: Duration) -> Result<String, String> {
    let mut enriched = body.clone();
    astra_tools::memoria::enrich_store_payload_with_views(&mut enriched);
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        "/memory/store",
        timeout,
        Some(&enriched),
    )
    .await
}

pub async fn memoria_correct(
    memory_id: &str,
    body: &Value,
    timeout: Duration,
) -> Result<String, String> {
    memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Put,
        &format!("/memory/correct/{memory_id}"),
        timeout,
        Some(body),
    )
    .await
}

impl ToolExecutor {
    fn memoria_record_service_failure(&self) {
        self.memoria_circuit.record_service_failure();
    }

    fn memoria_record_service_success(&self) {
        self.memoria_circuit.record_service_success();
        if self.memoria_notified_down.swap(false, Ordering::AcqRel) {
            eprintln!(
                "  {} Memoria memory service reconnected.",
                crossterm::style::Stylize::green("✓"),
            );
        }
    }

    /// Returns true while degraded. After the cooldown, one caller receives a
    /// half-open probe lease so a transient outage cannot disable memory for
    /// the lifetime of the CLI process.
    fn memoria_circuit_open(&self) -> bool {
        self.memoria_circuit.is_open()
    }

    pub(super) async fn memoria_call(&self, op: &str, args: &Value) -> String {
        self.memoria_call_with_timeout(op, args, Duration::from_secs(10))
            .await
    }

    pub(super) async fn memoria_call_with_timeout(
        &self,
        op: &str,
        args: &Value,
        timeout: Duration,
    ) -> String {
        // Local focus state and deterministic argument errors do not depend on
        // remote availability. Keep them usable/actionable during an outage.
        if op == "focus" {
            return astra_tools::memoria::MemoriaToolGateway::new(
                self.cloud_base.clone(),
                self.cloud_token(),
            )
            .call_with_timeout(op, args, timeout)
            .await;
        }
        if let Some(error) =
            astra_tools::memoria::MemoriaToolGateway::validate_before_side_effects(op, args)
        {
            return error.to_string();
        }

        let cloud_token = match self.cloud_token() {
            Some(token) => token,
            None => {
                return json!({
                    "error": "Memory unavailable: login required because CLI memory calls must go through the Astra server",
                    "hint": "Run `astra login` so the memory tool can use the authenticated server proxy"
                })
                .to_string();
            }
        };

        // CLI-specific, process-lived circuit breaker with user notification.
        if self.memoria_circuit_open() {
            if !self.memoria_notified_down.swap(true, Ordering::AcqRel) {
                eprintln!(
                    "  {} Memoria memory service is temporarily unreachable — calls \
                     will probe again after a short cooldown. Check /info if it persists.",
                    crossterm::style::Stylize::yellow("⚠"),
                );
            }
            return json!({"error": "Memory service unavailable (circuit open)"}).to_string();
        }

        // Delegate to the shared MemoriaPort (single source of truth for
        // build_direct_request, type normalization, and HTTP method routing).
        let client = astra_tools::memoria::MemoriaToolGateway::new(
            self.cloud_base.clone(),
            Some(cloud_token),
        );
        let result = client.call_with_timeout(op, args, timeout).await;

        // Only availability failures feed the circuit. Validation, auth, and
        // content errors remain visible to the caller without being amplified
        // into a session-wide memory outage.
        if astra_tools::memoria::memory_output_indicates_service_failure(&result) {
            self.memoria_record_service_failure();
        } else if astra_tools::memoria::memory_output_proves_service_reachable(&result) {
            self.memoria_record_service_success();
        }
        result
    }
}

#[cfg(test)]
mod circuit_contract_tests {
    use super::*;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn missing_memory_ids_remain_visible_without_poisoning_cli_service_health() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/memory/snapshots"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/memory/correct/missing-[12]$"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": "not found"
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/memory/correct/real-memory-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed"})))
            .mount(&server)
            .await;

        let temp = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(temp.path()).with_cloud(server.uri(), "test-token");
        for attempt in 1..=2 {
            let output = executor
                .memoria_call_with_timeout(
                    "update",
                    &json!({
                        "memory_id": format!("missing-{attempt}"),
                        "content": "replacement",
                        "reason": "exercise missing identity contract",
                    }),
                    Duration::from_secs(2),
                )
                .await;
            let value: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(value["error"]["code"], "memory_not_found");
            assert!(!executor.memoria_circuit.is_degraded());
        }

        let output = executor
            .memoria_call_with_timeout(
                "update",
                &json!({
                    "memory_id": "real-memory-id",
                    "content": "replacement",
                    "reason": "prove the next valid call remains available",
                }),
                Duration::from_secs(2),
            )
            .await;
        let value: Value = serde_json::from_str(&output).unwrap();
        assert!(value.get("error").is_none(), "valid update failed: {value}");
        assert!(!executor.memoria_circuit.is_degraded());
    }
}

// HttpMethod + build_direct_request moved to astra_tools::memoria::MemoriaToolGateway
// (single source of truth for CLI and server).
use astra_tools::memoria::HttpMethod;

fn build_direct_request(base: &str, op: &str, args: &Value) -> (String, Value, HttpMethod) {
    astra_tools::memoria::MemoriaToolGateway::build_direct_request(base, op, args)
}

// Old build_direct_request body (120 lines) removed.

#[cfg(test)]
mod build_direct_request_tests {
    use super::{
        build_direct_request, memoria_branch_create, memoria_health, memoria_reflect,
        memoria_snapshot_create,
    };
    use serde_json::json;

    #[test]
    fn retrieve_forwards_session_id() {
        let args = json!({
            "query": "test query",
            "top_k": 5,
            "session_id": "sess-123",
        });
        let (endpoint, pl, _) = build_direct_request("http://mem", "recall", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
        assert_eq!(pl["session_id"], "sess-123");
        assert_eq!(pl["query"], "test query");
    }

    #[test]
    fn recall_scope_session_requires_session_id() {
        // v2 `scope="session"` → v1 `session_scope="only"`; must refuse
        // when no session_id is available so the caller catches the bug
        // instead of silently downgrading to unscoped retrieval.
        let args = json!({"query": "test query", "top_k": 5, "scope": "session"});
        let (endpoint, pl, _) = build_direct_request("http://mem", "recall", &args);
        assert!(endpoint.is_empty(), "must short-circuit without session_id");
        assert!(
            pl.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("session_id"),
            "error must mention missing session_id"
        );
    }

    #[test]
    fn recall_scope_session_sets_session_scope_only() {
        let args = json!({
            "query": "test",
            "top_k": 5,
            "session_id": "sess-abc",
            "scope": "session",
        });
        let (endpoint, pl, _) = build_direct_request("http://mem", "recall", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
        assert_eq!(pl["session_id"], "sess-abc");
        assert_eq!(
            pl["session_scope"], "only",
            "v2 scope=session must map to v1 session_scope=only"
        );
    }

    #[test]
    fn recall_scope_all_omits_session_scope() {
        // `scope="all"` (or missing) → let v1 default ("prefer" when
        // session_id is present; unscoped otherwise) apply. Must NOT
        // emit a `session_scope` field.
        let args = json!({"query": "test", "top_k": 10, "scope": "all"});
        let (_, pl, _) = build_direct_request("http://mem", "recall", &args);
        assert!(pl.get("session_scope").is_none());
    }

    #[test]
    fn recall_omits_session_fields_when_absent() {
        let args = json!({"query": "test", "top_k": 10});
        let (_, pl, _) = build_direct_request("http://mem", "recall", &args);
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("session_scope").is_none());
    }

    #[test]
    fn recall_endpoint_is_v1_memories_retrieve() {
        let args = json!({"query": "test", "top_k": 10});
        let (endpoint, _, _) = build_direct_request("http://mem", "recall", &args);
        assert_eq!(
            endpoint, "http://mem/v1/memories/retrieve",
            "recall must route to /v1/memories/retrieve (the hybrid path)"
        );
    }

    #[test]
    fn store_forwards_session_id_and_trust_tier() {
        let args = json!({
            "content": "hello",
            "session_id": "sess-42",
            "trust_tier": "T1",
        });
        let (endpoint, pl, _) = build_direct_request("http://mem", "remember", &args);
        assert_eq!(endpoint, "http://mem/v1/memories");
        assert_eq!(pl["session_id"], "sess-42");
        assert_eq!(pl["trust_tier"], "T1");
    }

    #[test]
    fn store_maps_business_type_to_memoria_primitive() {
        let args = json!({"content": "preference", "memory_type": "feedback"});
        let (_, pl, _) = build_direct_request("http://mem", "remember", &args);
        assert_eq!(
            pl["memory_type"], "semantic",
            "business type 'feedback' must map to 'semantic'"
        );
    }

    #[test]
    fn store_passes_through_valid_memoria_types() {
        let args = json!({"content": "test", "memory_type": "profile"});
        let (_, pl, _) = build_direct_request("http://mem", "remember", &args);
        assert_eq!(pl["memory_type"], "profile");
    }

    // ── Cloud helpers now in astra-tools (single source of truth) ──

    #[test]
    fn cloud_helpers_are_re_exported_from_shared() {
        // These re-exports prove the shared module exposes all cloud helpers.
        // If any is removed from astra-tools, this file won't compile.
        let _ = memoria_snapshot_create;
        let _ = memoria_branch_create;
        let _ = memoria_reflect;
        let _ = memoria_health;
    }
}
