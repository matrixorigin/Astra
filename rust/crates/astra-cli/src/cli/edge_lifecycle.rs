//! Cloud edge registry + heartbeat (Phase 3). See `docs/design/multi-agent-cloud-runtime.md` §5.5.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::cli::chat_stream::edge_executor_instance_id;
use crate::cli::session_runtime::{attempt_token_refresh, current_access_token};
use astra_thin_client::{
    EdgeHeartbeatRequest, EdgeRegisterRequest, ThinClient, ThinClientError,
};
use astra_thin_client::edge::edge_register_with_capabilities;

/// Ring buffer of recently completed tool request IDs, for deduplication
/// on reconnection. Heartbeat sends these so cloud knows which tool calls
/// this edge already completed.
static COMPLETED_REQUEST_IDS: std::sync::LazyLock<Mutex<Vec<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::with_capacity(64)));

/// Maximum number of completed request IDs to track for deduplication.
const MAX_COMPLETED_REQUEST_IDS: usize = 64;

/// Record a recently completed tool request ID for heartbeat dedup.
pub fn record_completed_request(request_id: String) {
    if let Ok(mut ids) = COMPLETED_REQUEST_IDS.lock() {
        if ids.len() >= MAX_COMPLETED_REQUEST_IDS {
            ids.remove(0);
        }
        ids.push(request_id);
    }
}

/// Snapshot of completed request IDs (cloned for heartbeat).
fn completed_request_ids_snapshot() -> Vec<String> {
    COMPLETED_REQUEST_IDS
        .lock()
        .map(|ids| ids.clone())
        .unwrap_or_default()
}

/// Global counter of in-flight tool requests on this edge executor.
/// Incremented before tool dispatch, decremented on result (success or error).
/// Read by heartbeat to populate `pending_request_count`.
static PENDING_TOOL_REQUESTS: AtomicU32 = AtomicU32::new(0);

/// Increment the pending tool request counter.
pub fn inc_pending_tool_requests() {
    PENDING_TOOL_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

/// Decrement the pending tool request counter.
pub fn dec_pending_tool_requests() {
    PENDING_TOOL_REQUESTS.fetch_sub(1, Ordering::Relaxed);
}

/// When `ASTRA_EDGE_REGISTRY` is `0`, `false`, or `off`, skip register and heartbeat.
pub fn edge_cloud_registry_enabled() -> bool {
    !matches!(
        std::env::var("ASTRA_EDGE_REGISTRY").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

// ── backoff helpers ────────────────────────────────────────────────────────

/// Returns `delay_secs` with a random jitter in [0, 500] ms.
fn jitter(delay: Duration) -> Duration {
    use std::time::UNIX_EPOCH;
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let jitter_ms = (nanos / 1000) % 500; // deterministic per-ms, good enough
    delay + Duration::from_millis(jitter_ms as u64)
}

/// Exponential backoff sequence with jitter, capped at 30 s.
fn backoff_delay(consecutive_failures: u32) -> Duration {
    let base: u64 = match consecutive_failures {
        0 => 1,
        1 => 2,
        2 => 4,
        3 => 8,
        4 => 16,
        _ => 30,
    };
    jitter(Duration::from_secs(base))
}

/// Determine heartbeat interval from env `ASTRA_EDGE_HEARTBEAT_SECS` (default 120).
/// Returns `None` when set to `0` (heartbeat disabled).
fn heartbeat_period() -> Option<Duration> {
    let secs: u64 = std::env::var("ASTRA_EDGE_HEARTBEAT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

fn enrich_register_body(body: &mut EdgeRegisterRequest) {
    if body.hostname.is_none() {
        body.hostname = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("COMPUTERNAME").ok());
    }
    if body.worktree_path.is_none()
        && let Ok(cwd) = std::env::current_dir()
    {
        body.worktree_path = cwd.to_str().map(String::from);
    }
}

pub async fn register_edge_once(api: &ThinClient, token: &str) -> Result<(), ThinClientError> {
    if !edge_cloud_registry_enabled() {
        return Ok(());
    }
    let transport_id = edge_executor_instance_id();
    let mut body = edge_register_with_capabilities(transport_id);
    enrich_register_body(&mut body);
    api.post_agents_edge_register(Some(token), Some(transport_id), &body)
        .await?;
    Ok(())
}

async fn send_heartbeat(api: &ThinClient, token: &str) -> Result<Option<Vec<serde_json::Value>>, ThinClientError> {
    if !edge_cloud_registry_enabled() {
        return Ok(None);
    }
    let id = edge_executor_instance_id();
    let hb = EdgeHeartbeatRequest {
        edge_agent_id: id.to_string(),
        pending_request_count: PENDING_TOOL_REQUESTS.load(Ordering::Relaxed),
        last_seen_request_ids: completed_request_ids_snapshot(),
    };
    let resp = api
        .post_agents_edge_heartbeat(Some(token), Some(id), &hb)
        .await?;

    // Parse pending_requests from heartbeat response (reconnection dedup)
    let pending = resp
        .get("pending_requests")
        .and_then(|v| v.as_array())
        .cloned();
    Ok(pending)
}

pub fn spawn_edge_heartbeat(
    api: ThinClient,
    token: String,
    profile: Option<String>,
) -> Option<tokio::task::JoinHandle<()>> {
    let period = heartbeat_period()?;
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.tick().await; // skip immediate first tick — register just happened
        let mut token = token;
        let mut failures: u32 = 0;
        loop {
            interval.tick().await;
            match send_heartbeat(&api, &token).await {
                Ok(Some(pending_requests)) => {
                    failures = 0;
                    // ── Reconnection dedup: re-execute pending tools ──
                    if !pending_requests.is_empty() {
                        let client = api.clone();
                        let t = token.clone();
                        let id = edge_executor_instance_id().to_string();
                        tokio::spawn(async move {
                            reexecute_pending_requests(&client, &t, &id, &pending_requests)
                                .await;
                        });
                    }
                }
                Ok(None) => {
                    failures = 0;
                }
                Err(e) if is_unauthorized(&e) => {
                    if attempt_token_refresh(&api, profile.as_deref()).await
                        && let Some(fresh) = current_access_token(profile.as_deref())
                    {
                        token = fresh;
                        failures = 0;
                    } else {
                        return;
                    }
                }
                Err(_) => {
                    failures += 1;
                    let delay = backoff_delay(failures);
                    tracing::warn!(
                        target: "astra.edge.heartbeat",
                        consecutive_failures = failures,
                        backoff_secs = delay.as_secs_f64(),
                        "heartbeat failed, backing off"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }))
}

/// Re-execute pending tool requests that the cloud returned after reconnection.
///
/// This is the dedup mechanism: when an edge reconnects, the cloud heartbeat
/// response includes any `pending_requests` that were dispatched while the
/// edge was disconnected. The edge re-executes them and reports results back.
async fn reexecute_pending_requests(
    api: &ThinClient,
    token: &str,
    executor_id: &str,
    pending: &[serde_json::Value],
) {
    for req in pending {
        let request_id = match req.get("request_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let tool_name = match req.get("tool_name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => continue,
        };
        let args = req.get("args").cloned().unwrap_or(serde_json::Value::Null);

        tracing::info!(
            target: "astra.edge.reconnect",
            request_id = %request_id,
            tool = tool_name,
            "reconnection: re-executing pending tool"
        );

        // TODO: Actually execute the tool locally via the tool dispatch system.
        // For now, log the intent — the full integration requires wiring into
        // the tool executor to run the tool and call `post_tool_result` back
        // to cloud with the result.
        let _ = (api, token, executor_id, request_id, tool_name, args);
    }
}

/// Register with the cloud (best-effort) and start a background heartbeat task.
/// Returns `None` when registry is disabled, register failed, or heartbeat interval is `0`.
///
/// On a 401 from the first register attempt, silently refresh the
/// token and retry once — the same recovery chat uses. Eliminates
/// the noisy startup "Edge registry skipped (HTTP 401)" banner that
/// was firing whenever `try_silent_auth` couldn't reach the refresh
/// endpoint but the token happened to work later for chat.
pub async fn register_and_start_heartbeat(
    api: &ThinClient,
    token: &str,
    profile: Option<&str>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !edge_cloud_registry_enabled() {
        return None;
    }
    let (final_token, success) = match register_edge_once(api, token).await {
        Ok(()) => (token.to_string(), true),
        Err(ref e) if is_unauthorized(e) => {
            // Access token is stale. Try one silent refresh and retry.
            if attempt_token_refresh(api, profile).await {
                if let Some(fresh) = current_access_token(profile) {
                    match register_edge_once(api, &fresh).await {
                        Ok(()) => (fresh, true),
                        Err(retry_err) => {
                            print_skip_notice(&retry_err);
                            return None;
                        }
                    }
                } else {
                    print_skip_notice(e);
                    return None;
                }
            } else {
                print_skip_notice(e);
                return None;
            }
        }
        Err(e) => {
            print_skip_notice(&e);
            return None;
        }
    };
    // Cloud status is shown in the startup card — no separate line.
    let _ = success;
    spawn_edge_heartbeat(api.clone(), final_token, profile.map(str::to_owned))
}

/// True when a `ThinClientError` represents a server-side 401.
fn is_unauthorized(err: &ThinClientError) -> bool {
    matches!(
        err,
        ThinClientError::Api { status, .. } if *status == reqwest::StatusCode::UNAUTHORIZED
    )
}

fn print_skip_notice(_e: &ThinClientError) {
    // Cloud status shown in startup card — suppress individual notice.
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_thin_client::ASTRA_EDGE_ID_HEADER;
    use serial_test::serial;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `serial_test::serial` on callers avoids concurrent env access from other tests.
    fn env_set(key: &str, value: &str) {
        // SAFETY: only used from `#[serial]` tests in this module.
        unsafe { std::env::set_var(key, value) }
    }

    fn env_remove(key: &str) {
        // SAFETY: only used from `#[serial]` tests in this module.
        unsafe { std::env::remove_var(key) }
    }

    #[test]
    #[serial]
    fn edge_cloud_registry_enabled_respects_env() {
        let prev = std::env::var("ASTRA_EDGE_REGISTRY").ok();
        env_remove("ASTRA_EDGE_REGISTRY");
        assert!(edge_cloud_registry_enabled());
        env_set("ASTRA_EDGE_REGISTRY", "0");
        assert!(!edge_cloud_registry_enabled());
        env_set("ASTRA_EDGE_REGISTRY", "false");
        assert!(!edge_cloud_registry_enabled());
        env_set("ASTRA_EDGE_REGISTRY", "off");
        assert!(!edge_cloud_registry_enabled());
        match &prev {
            Some(v) => env_set("ASTRA_EDGE_REGISTRY", v),
            None => env_remove("ASTRA_EDGE_REGISTRY"),
        }
    }

    #[test]
    #[serial]
    fn heartbeat_period_parsing() {
        let prev = std::env::var("ASTRA_EDGE_HEARTBEAT_SECS").ok();
        env_remove("ASTRA_EDGE_HEARTBEAT_SECS");
        assert_eq!(heartbeat_period(), Some(Duration::from_secs(120)));
        env_set("ASTRA_EDGE_HEARTBEAT_SECS", "0");
        assert_eq!(heartbeat_period(), None);
        env_set("ASTRA_EDGE_HEARTBEAT_SECS", "30");
        assert_eq!(heartbeat_period(), Some(Duration::from_secs(30)));
        match &prev {
            Some(v) => env_set("ASTRA_EDGE_HEARTBEAT_SECS", v),
            None => env_remove("ASTRA_EDGE_HEARTBEAT_SECS"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn register_disabled_skips_http() {
        let prev_reg = std::env::var("ASTRA_EDGE_REGISTRY").ok();
        env_set("ASTRA_EDGE_REGISTRY", "0");
        let api = ThinClient::new("http://127.0.0.1:1", None).expect("url");
        let r = register_edge_once(&api, "token").await;
        assert!(r.is_ok());
        match &prev_reg {
            Some(v) => env_set("ASTRA_EDGE_REGISTRY", v),
            None => env_remove("ASTRA_EDGE_REGISTRY"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn register_edge_once_hits_wiremock() {
        env_remove("ASTRA_EDGE_REGISTRY");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .and(header_exists("authorization"))
            .and(header_exists(ASTRA_EDGE_ID_HEADER))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("url");
        register_edge_once(&api, "test-bearer")
            .await
            .expect("register");

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
        assert!(body.get("edge_agent_id").and_then(|v| v.as_str()).is_some());
        assert!(body.get("capabilities").is_some());
    }

    #[tokio::test]
    #[serial]
    async fn register_enriches_hostname_from_env() {
        let prev_host = std::env::var("HOSTNAME").ok();
        let prev_reg = std::env::var("ASTRA_EDGE_REGISTRY").ok();
        env_set("HOSTNAME", "unit-test-host");
        env_remove("ASTRA_EDGE_REGISTRY");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("url");
        register_edge_once(&api, "t").await.expect("register");

        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
        assert_eq!(
            body.get("hostname").and_then(|v| v.as_str()),
            Some("unit-test-host")
        );

        match &prev_host {
            Some(v) => env_set("HOSTNAME", v),
            None => env_remove("HOSTNAME"),
        }
        match &prev_reg {
            Some(v) => env_set("ASTRA_EDGE_REGISTRY", v),
            None => env_remove("ASTRA_EDGE_REGISTRY"),
        }
    }

    // ── backoff / heartbeat tests ───────────────────────────────────────────

    #[test]
    fn backoff_delay_sequence() {
        // Without env jitter overrides, verify base delays are correct.
        // (Jitter adds 0-500ms, so we check the floor.)
        assert!(backoff_delay(0) >= Duration::from_secs(1));
        assert!(backoff_delay(1) >= Duration::from_secs(2));
        assert!(backoff_delay(2) >= Duration::from_secs(4));
        assert!(backoff_delay(3) >= Duration::from_secs(8));
        assert!(backoff_delay(4) >= Duration::from_secs(16));
        assert!(backoff_delay(5) >= Duration::from_secs(30));
        assert!(backoff_delay(100) >= Duration::from_secs(30));
        // Jitter < 500ms
        assert!(backoff_delay(0) < Duration::from_millis(1500));
        assert!(backoff_delay(5) < Duration::from_millis(30500));
    }

    #[test]
    #[serial]
    fn pending_request_counter() {
        // Reset to known state
        PENDING_TOOL_REQUESTS.store(0, Ordering::Relaxed);
        assert_eq!(PENDING_TOOL_REQUESTS.load(Ordering::Relaxed), 0);

        inc_pending_tool_requests();
        inc_pending_tool_requests();
        assert_eq!(PENDING_TOOL_REQUESTS.load(Ordering::Relaxed), 2);

        dec_pending_tool_requests();
        assert_eq!(PENDING_TOOL_REQUESTS.load(Ordering::Relaxed), 1);

        dec_pending_tool_requests();
        assert_eq!(PENDING_TOOL_REQUESTS.load(Ordering::Relaxed), 0);

        // Underflow saturates at 0 (AtomicU32 wrapping is UB, but fetch_sub on 0 → max)
        // Rather than test wrapping, just reset to 0.
        PENDING_TOOL_REQUESTS.store(0, Ordering::Relaxed);
    }

    #[tokio::test]
    #[serial]
    async fn heartbeat_includes_pending_count() {
        env_remove("ASTRA_EDGE_REGISTRY");
        PENDING_TOOL_REQUESTS.store(3, Ordering::Relaxed);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge/heartbeat"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("url");
        send_heartbeat(&api, "test-token")
            .await
            .expect("heartbeat");

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("json body");
        assert_eq!(
            body.get("pending_request_count")
                .and_then(|v| v.as_u64()),
            Some(3)
        );
        // last_seen_request_ids present (may be empty or contain prior completions)
        assert!(
            body.get("last_seen_request_ids")
                .and_then(|v| v.as_array())
                .is_some(),
            "heartbeat must include last_seen_request_ids"
        );

        PENDING_TOOL_REQUESTS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn completed_request_ids_ring_buffer() {
        // Clear state
        if let Ok(mut ids) = COMPLETED_REQUEST_IDS.lock() {
            ids.clear();
        }
        for i in 0..70 {
            record_completed_request(format!("req-{i}"));
        }
        let snapshot = completed_request_ids_snapshot();
        assert_eq!(snapshot.len(), 64, "ring buffer caps at 64 entries");
        // Oldest entries evicted
        assert!(!snapshot.contains(&"req-0".to_string()));
        // Newest entries preserved
        assert!(snapshot.contains(&"req-69".to_string()));
    }
}
