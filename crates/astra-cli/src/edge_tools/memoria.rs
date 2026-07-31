//! Memoria (memory service) integration for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use super::ToolExecutor;

pub use astra_tools::memoria::{BoostSearchHit, parse_memory_search_hits};

pub(crate) const MEMORY_BOOST_MIN_SCORE: f64 = 0.3;

pub(crate) fn filter_memory_boost_hits_for_prompt(
    hits: Vec<BoostSearchHit>,
) -> Vec<BoostSearchHit> {
    hits.into_iter()
        .filter(|hit| {
            hit.score
                .is_some_and(|score| score.is_finite() && score >= MEMORY_BOOST_MIN_SCORE)
        })
        .collect()
}

#[cfg(test)]
mod memory_boost_prompt_filter_tests {
    use super::{BoostSearchHit, filter_memory_boost_hits_for_prompt};

    #[test]
    fn memory_boost_prompt_filter_rejects_low_missing_and_nan_scores() {
        let hits = vec![
            BoostSearchHit {
                memory_id: Some("low".into()),
                content: "low relevance".into(),
                score: Some(0.137),
            },
            BoostSearchHit {
                memory_id: Some("missing".into()),
                content: "missing score".into(),
                score: None,
            },
            BoostSearchHit {
                memory_id: Some("nan".into()),
                content: "nan score".into(),
                score: Some(f64::NAN),
            },
            BoostSearchHit {
                memory_id: Some("ok".into()),
                content: "high relevance".into(),
                score: Some(0.3),
            },
        ];

        let filtered = filter_memory_boost_hits_for_prompt(hits);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].memory_id.as_deref(), Some("ok"));
    }
}

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
    let (base, token) = current_memoria_proxy_target()?;
    let client = astra_core::net::client_builder_for_target(&base)
        .timeout(timeout)
        .build()
        .map_err(|e| format!("build client: {e}"))?;
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

pub async fn memoria_governance_fire_and_forget() {
    let _ = memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        "/memory/governance",
        Duration::from_secs(10),
        Some(&json!({ "force": false })),
    )
    .await;
}

pub async fn memoria_consolidate_fire_and_forget() {
    let _ = memoria_proxy_request(
        astra_tools::memoria::HttpMethod::Post,
        "/memory/consolidate",
        Duration::from_secs(15),
        Some(&json!({ "force": false })),
    )
    .await;
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

    pub async fn memory_boost_search(&self, query: &str, top_k: u64) -> Vec<BoostSearchHit> {
        if query.trim().is_empty() {
            return vec![];
        }
        if self.memoria_circuit_open() {
            return vec![];
        }
        let cloud_base = match self.cloud_base.as_deref() {
            Some(base) => base,
            None => return vec![],
        };
        let token = match self.cloud_token() {
            Some(token) => token,
            None => return vec![],
        };
        let client = match astra_core::net::client_builder_for_target(&cloud_base)
            .timeout(Duration::from_millis(800))
            .build()
        {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        match client
            .post(format!("{cloud_base}/memory/retrieve"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&json!({
                "query": query,
                "top_k": top_k,
                "min_confidence": 0.3
            }))
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                match resp.text().await {
                    Ok(text) if status.is_success() => {
                        self.memoria_record_service_success();
                        filter_memory_boost_hits_for_prompt(parse_memory_search_hits(&text))
                    }
                    Ok(_)
                        if status.is_server_error()
                            || matches!(
                                status,
                                reqwest::StatusCode::REQUEST_TIMEOUT
                                    | reqwest::StatusCode::TOO_MANY_REQUESTS
                            ) =>
                    {
                        self.memoria_record_service_failure();
                        vec![]
                    }
                    Ok(_) => {
                        self.memoria_record_service_success();
                        vec![]
                    }
                    Err(_) => {
                        self.memoria_record_service_failure();
                        vec![]
                    }
                }
            }
            Err(_) => {
                self.memoria_record_service_failure();
                vec![]
            }
        }
    }

    /// Fire-and-forget: send "useful" feedback for retrieved memory IDs.
    ///
    /// Called after boost search results are injected into the prompt.
    /// Spawns a background task — does not block the caller.
    pub fn memory_feedback_useful(&self, memory_ids: Vec<String>) {
        if memory_ids.is_empty() {
            return;
        }
        if self.memoria_circuit.is_degraded() {
            return;
        }
        let Some(cloud_base) = self.cloud_base.clone() else {
            return;
        };
        let token = match self.cloud_token() {
            Some(token) => token,
            None => return,
        };
        tokio::spawn(async move {
            let client = match astra_core::net::client_builder_for_target(&cloud_base)
                .timeout(Duration::from_secs(5))
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };
            for mid in memory_ids {
                let url = format!("{cloud_base}/memory/feedback/{mid}");
                if let Err(e) = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {token}"))
                    .json(&json!({
                        "signal": "useful",
                        "context": "boost_search retrieval"
                    }))
                    .send()
                    .await
                {
                    eprintln!("[memoria] feedback for {mid} failed: {e}");
                    break; // don't spam on persistent failures
                }
            }
        });
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

/// Retrieve procedural/semantic lessons from Memoria for session bootstrap.
/// `context_query` should be derived from the user's first message — this
/// produces much better semantic retrieval than keyword stuffing.
/// Returns canonical lesson hints. Best-effort: returns empty vec
/// on any error (circuit breaker, timeout, parse failure).
pub async fn memoria_retrieve_lessons(
    top_k: u64,
    context_query: Option<&str>,
) -> Vec<astra_services::LessonHint> {
    let query = context_query.unwrap_or("reusable lessons and corrections from prior sessions");
    let payload = json!({
        "query": query,
        "top_k": top_k,
        "min_confidence": 0.3,
    });
    let text = match memoria_retrieve(&payload, Duration::from_secs(3)).await {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    // Memoria /v1/memories/retrieve returns a direct array when explain
    // is off, or {"results": [...]} when explain is on. Handle both.
    let memories = if let Some(arr) = value.as_array() {
        arr
    } else if let Some(arr) = value.get("memories").and_then(|v| v.as_array()) {
        arr
    } else if let Some(arr) = value.get("results").and_then(|v| v.as_array()) {
        arr
    } else {
        return Vec::new();
    };
    memories
        .iter()
        .filter_map(astra_services::memory_value_to_lesson_hint)
        .collect()
}

/// Store extracted lessons in Memoria as L3 durable memory.
///
/// Best-effort and loss-tolerant: lessons are sent one-by-one through the
/// server proxy, but a failure on one lesson must not drop the rest.
pub async fn memoria_store_lessons_fire_and_forget(
    lessons: Vec<astra_runtime::learning::synthesizer::ExtractedLesson>,
    session_id: Option<String>,
) {
    if lessons.is_empty() {
        return;
    }
    let mut stored = 0usize;
    let mut failed = 0usize;
    for lesson in lessons {
        let mut body = json!({
            "content": lesson.content,
            "memory_type": lesson.memory_type,
            "trust_tier": lesson.trust_tier,
            "source": {"agent": "session_end"},
        });
        if let Some(ref sid) = session_id {
            body["session_id"] = json!(sid);
        }
        match memoria_store(&body, Duration::from_secs(5)).await {
            Ok(_) => stored += 1,
            Err(e) => {
                failed += 1;
                tracing::debug!(
                    target: "memoria",
                    error = %e,
                    "session-end lesson store failed",
                );
            }
        }
    }
    if failed > 0 {
        tracing::warn!(
            target: "memoria",
            stored,
            failed,
            "session-end lesson store completed with partial failures"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::memoria_store_lessons_fire_and_forget;
    use serial_test::serial;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_ref() {
                unsafe { std::env::set_var(self.key, previous) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn session_end_lesson_store_continues_after_first_failure() {
        let server = MockServer::start().await;
        let _api = EnvGuard::set("ASTRA_API_URL", &server.uri());
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "test-token");

        Mock::given(method("POST"))
            .and(path("/memory/store"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory/store"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(2)
            .mount(&server)
            .await;

        memoria_store_lessons_fire_and_forget(
            vec![
                astra_runtime::learning::synthesizer::ExtractedLesson {
                    memory_type: "working",
                    content: "lesson one".into(),
                    trust_tier: "T4",
                },
                astra_runtime::learning::synthesizer::ExtractedLesson {
                    memory_type: "working",
                    content: "lesson two".into(),
                    trust_tier: "T4",
                },
                astra_runtime::learning::synthesizer::ExtractedLesson {
                    memory_type: "working",
                    content: "lesson three".into(),
                    trust_tier: "T4",
                },
            ],
            Some("sess-1".into()),
        )
        .await;

        let requests = server.received_requests().await.expect("captured requests");
        assert_eq!(
            requests.len(),
            3,
            "must continue storing after first failure"
        );
    }
}

// Cloud memory helpers (snapshot, branch, reflect, health) now live in
// astra_tools::memoria — re-exported at the top of this file.
