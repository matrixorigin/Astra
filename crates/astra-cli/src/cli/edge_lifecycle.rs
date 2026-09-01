//! Cloud edge registry + heartbeat (Phase 3). See `docs/design/multi-agent-cloud-runtime.md` §5.5.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::cli::chat_stream::edge_executor_instance_id;
use crate::cli::session::session_runtime::{attempt_token_refresh, current_access_token};
use astra_thin_client::edge::edge_runtime_environment_capabilities;
use astra_thin_client::{
    EdgeHeartbeatRequest, EdgeHeartbeatResponse, EdgeRegisterRequest, ThinClient, ThinClientError,
};

/// Maximum number of completed request IDs to track for deduplication.
const MAX_COMPLETED_REQUEST_IDS: usize = 256;

/// Process-scoped edge lifecycle state shared by the heartbeat loop and all
/// edge-side SSE hosts within this CLI process.
#[derive(Default)]
struct EdgeLifecycleContext {
    completed_request_ids: Mutex<VecDeque<String>>,
    pending_tool_requests: AtomicU32,
    registered_worktree_path: Mutex<Option<PathBuf>>,
    last_reconciliation_signature: Mutex<Option<HeartbeatReconciliationSignature>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeartbeatReconciliationSignature {
    replay_policy: astra_thin_client::EdgeHeartbeatReplayPolicy,
    unresolved_request_ids: Vec<String>,
}

impl EdgeLifecycleContext {
    fn record_completed_request(&self, request_id: String) {
        if let Ok(mut ids) = self.completed_request_ids.lock() {
            if ids.len() >= MAX_COMPLETED_REQUEST_IDS {
                ids.pop_front();
            }
            ids.push_back(request_id);
        }
    }

    fn completed_request_ids_snapshot(&self) -> Vec<String> {
        self.completed_request_ids
            .lock()
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn inc_pending_tool_requests(&self) {
        self.pending_tool_requests.fetch_add(1, Ordering::AcqRel);
    }

    fn dec_pending_tool_requests(&self) {
        let _ =
            self.pending_tool_requests
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |val| {
                    if val > 0 { Some(val - 1) } else { None }
                });
    }

    fn pending_tool_request_count(&self) -> u32 {
        self.pending_tool_requests.load(Ordering::Acquire)
    }

    fn set_registered_worktree_path(&self, path: &Path) {
        if let Ok(mut guard) = self.registered_worktree_path.lock() {
            *guard = Some(path.to_path_buf());
        }
    }

    #[cfg(test)]
    fn registered_worktree_path(&self) -> Option<PathBuf> {
        self.registered_worktree_path
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn reconciliation_changed(&self, response: &EdgeHeartbeatResponse) -> bool {
        let mut unresolved_request_ids = response.unresolved_request_ids.clone();
        unresolved_request_ids.sort();
        unresolved_request_ids.dedup();
        let next = response
            .requires_reconciliation()
            .then_some(HeartbeatReconciliationSignature {
                replay_policy: response.replay_policy,
                unresolved_request_ids,
            });
        let Ok(mut previous) = self.last_reconciliation_signature.lock() else {
            // A poisoned diagnostics lock must not hide a correctness warning.
            return next.is_some();
        };
        if *previous == next {
            return false;
        }
        *previous = next;
        previous.is_some()
    }

    fn jitter(&self, delay: Duration) -> Duration {
        // Per-call entropy from the wall clock's nanosecond residue. The previous
        // shape used `self.jitter_clock.elapsed().as_millis() % 500`, which was
        // deterministic at any heartbeat period that's a multiple of 500ms (e.g.
        // the default 120s) — the modulo always landed on 0, defeating the
        // thundering-herd mitigation it was meant to provide.
        //
        // SystemTime is good enough here: we only need ~9 bits of entropy and
        // the cost is one syscall. Deliberately not introducing a `rand`
        // dependency just for jitter.
        //
        // We also mix in a per-thread monotonic counter so that jitter varies
        // even on platforms where the OS clock has coarse resolution (e.g.
        // macOS returns subsec_nanos in multiples of 1000).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        let seq = JITTER_SEQ.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(1));
            v
        });
        let jitter_ms = (nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15)) % 500;
        delay + Duration::from_millis(jitter_ms)
    }

    #[cfg(test)]
    fn reset_for_test(&self) {
        if let Ok(mut ids) = self.completed_request_ids.lock() {
            ids.clear();
        }
        self.pending_tool_requests.store(0, Ordering::Relaxed);
        if let Ok(mut guard) = self.registered_worktree_path.lock() {
            *guard = None;
        }
        if let Ok(mut signature) = self.last_reconciliation_signature.lock() {
            *signature = None;
        }
    }
}

static EDGE_LIFECYCLE: std::sync::LazyLock<EdgeLifecycleContext> =
    std::sync::LazyLock::new(EdgeLifecycleContext::default);

fn edge_lifecycle() -> &'static EdgeLifecycleContext {
    &EDGE_LIFECYCLE
}

/// Record a recently completed tool request ID for heartbeat dedup.
pub(crate) fn record_completed_request(request_id: String) {
    edge_lifecycle().record_completed_request(request_id);
}

fn completed_request_ids_snapshot() -> Vec<String> {
    edge_lifecycle().completed_request_ids_snapshot()
}

pub(crate) struct PendingToolRequestGuard<'a> {
    ctx: &'a EdgeLifecycleContext,
}

impl PendingToolRequestGuard<'static> {
    pub(crate) fn acquire() -> Self {
        let ctx = edge_lifecycle();
        ctx.inc_pending_tool_requests();
        Self { ctx }
    }
}

impl Drop for PendingToolRequestGuard<'_> {
    fn drop(&mut self) {
        self.ctx.dec_pending_tool_requests();
    }
}

/// When `ASTRA_EDGE_REGISTRY` is `0`, `false`, or `off`, skip register and heartbeat.
pub fn edge_cloud_registry_enabled() -> bool {
    !matches!(
        std::env::var("ASTRA_EDGE_REGISTRY").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

// ── backoff helpers ────────────────────────────────────────────────────────

// Per-thread counter to ensure jitter varies even when the OS clock has
// coarse resolution (e.g. macOS `subsec_nanos()` always returns multiples of
// 1000, making `nanos % 500` a constant zero).
thread_local! {
    static JITTER_SEQ: Cell<u64> = const { Cell::new(0) };
}

/// Returns `delay_secs` with a random jitter in [0, 500] ms.
fn jitter(delay: Duration) -> Duration {
    edge_lifecycle().jitter(delay)
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
    if let Some(ref worktree_path) = body.worktree_path {
        edge_lifecycle().set_registered_worktree_path(Path::new(worktree_path));
    }
}

fn attach_runtime_environment_capabilities(body: &mut EdgeRegisterRequest) {
    let Some(worktree_path) = body.worktree_path.clone() else {
        body.capabilities = None;
        return;
    };
    body.capabilities = Some(edge_runtime_environment_capabilities(
        &body.edge_agent_id,
        worktree_path,
    ));
}

pub async fn register_edge_once(api: &ThinClient, token: &str) -> Result<(), ThinClientError> {
    if !edge_cloud_registry_enabled() {
        return Ok(());
    }
    let transport_id = edge_executor_instance_id();
    let mut body = EdgeRegisterRequest::new(transport_id);
    enrich_register_body(&mut body);
    attach_runtime_environment_capabilities(&mut body);
    api.post_agents_edge_register(Some(token), Some(transport_id), &body)
        .await?;
    Ok(())
}

async fn send_heartbeat(
    api: &ThinClient,
    token: &str,
) -> Result<Option<EdgeHeartbeatResponse>, ThinClientError> {
    if !edge_cloud_registry_enabled() {
        return Ok(None);
    }
    let id = edge_executor_instance_id();
    let hb = EdgeHeartbeatRequest {
        edge_agent_id: id.to_string(),
        pending_request_count: edge_lifecycle().pending_tool_request_count(),
        last_seen_request_ids: completed_request_ids_snapshot(),
    };
    let response = api
        .post_agents_edge_heartbeat(Some(token), Some(id), &hb)
        .await?;

    Ok(Some(response))
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
                Ok(Some(reconciliation)) => {
                    failures = 0;
                    if reconciliation.requires_reconciliation() {
                        if edge_lifecycle().reconciliation_changed(&reconciliation) {
                            tracing::error!(
                                target: "astra.edge.reconnect",
                                unresolved_request_ids = ?reconciliation.unresolved_request_ids,
                                replay_policy = ?reconciliation.replay_policy,
                                "edge heartbeat reported unresolved invocations; automatic tool re-execution is forbidden without durable result evidence"
                            );
                        } else {
                            tracing::trace!(
                                target: "astra.edge.reconnect",
                                unresolved_count = reconciliation.unresolved_request_ids.len(),
                                "edge heartbeat reconciliation state is unchanged"
                            );
                        }
                    } else {
                        edge_lifecycle().reconciliation_changed(&reconciliation);
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
    use super::{
        EdgeLifecycleContext, PendingToolRequestGuard, attach_runtime_environment_capabilities,
        backoff_delay, completed_request_ids_snapshot, edge_cloud_registry_enabled, edge_lifecycle,
        enrich_register_body, heartbeat_period, jitter, record_completed_request,
        register_edge_once, send_heartbeat,
    };
    use astra_thin_client::{
        ASTRA_EDGE_ID_HEADER, EdgeHeartbeatReplayPolicy, EdgeHeartbeatResponse,
        EdgeRegisterRequest, ThinClient,
    };
    use serial_test::serial;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
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

    /// Regression: the old jitter implementation used
    /// `(jitter_clock.elapsed().as_millis() % 500)`, which was deterministic
    /// at any heartbeat period that was a multiple of 500ms (the default 120s
    /// landed on `jitter_ms == 0` every cycle). Sample 16 calls and assert at
    /// least two distinct jitter values — a defective constant-jitter would
    /// always return the same one.
    #[test]
    fn jitter_varies_across_calls() {
        let mut samples = std::collections::HashSet::new();
        for _ in 0..16 {
            samples.insert(jitter(Duration::from_secs(0)).as_millis());
            // Spin briefly so SystemTime nanos advance (~µs resolution suffices).
            std::thread::sleep(Duration::from_micros(50));
        }
        assert!(
            samples.len() >= 2,
            "jitter must vary across calls (saw {} unique values)",
            samples.len()
        );
    }

    #[test]
    #[serial]
    fn pending_request_counter() {
        let ctx = edge_lifecycle();
        ctx.pending_tool_requests.store(0, Ordering::Relaxed);
        assert_eq!(ctx.pending_tool_requests.load(Ordering::Relaxed), 0);

        ctx.inc_pending_tool_requests();
        ctx.inc_pending_tool_requests();
        assert_eq!(ctx.pending_tool_requests.load(Ordering::Relaxed), 2);

        ctx.dec_pending_tool_requests();
        assert_eq!(ctx.pending_tool_requests.load(Ordering::Relaxed), 1);

        ctx.dec_pending_tool_requests();
        assert_eq!(ctx.pending_tool_requests.load(Ordering::Relaxed), 0);

        ctx.dec_pending_tool_requests();
        assert_eq!(
            ctx.pending_tool_requests.load(Ordering::Relaxed),
            0,
            "counter must saturate at zero instead of wrapping"
        );
    }

    #[tokio::test]
    #[serial]
    async fn heartbeat_includes_pending_count() {
        env_remove("ASTRA_EDGE_REGISTRY");
        let ctx = edge_lifecycle();
        ctx.pending_tool_requests.store(3, Ordering::Relaxed);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge/heartbeat"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "user_id": "user-1",
                "edge_id": "transport-1",
                "edge_agent_id": "edge-1",
                "unresolved_request_ids": [],
                "replay_policy": "durable_result_reconciliation_required",
                "ack_request_ids": []
            })))
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("url");
        send_heartbeat(&api, "test-token").await.expect("heartbeat");

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
        assert_eq!(
            body.get("pending_request_count").and_then(|v| v.as_u64()),
            Some(3)
        );
        // last_seen_request_ids present (may be empty or contain prior completions)
        assert!(
            body.get("last_seen_request_ids")
                .and_then(|v| v.as_array())
                .is_some(),
            "heartbeat must include last_seen_request_ids"
        );

        ctx.pending_tool_requests.store(0, Ordering::Relaxed);
    }

    #[test]
    fn heartbeat_reconciliation_requires_durable_identity() {
        let reconciliation = EdgeHeartbeatResponse {
            ok: true,
            user_id: "user-1".to_string(),
            edge_id: "transport-1".to_string(),
            edge_agent_id: "edge-1".to_string(),
            unresolved_request_ids: vec!["invocation-new".to_string()],
            replay_policy: EdgeHeartbeatReplayPolicy::DurableResultReconciliationRequired,
            ack_request_ids: Vec::new(),
        };

        assert_eq!(
            reconciliation.unresolved_request_ids,
            vec!["invocation-new"]
        );
        assert!(reconciliation.requires_reconciliation());
    }

    #[test]
    fn reconciliation_errors_only_when_the_unresolved_state_changes() {
        let ctx = EdgeLifecycleContext::default();
        let unresolved = EdgeHeartbeatResponse {
            ok: true,
            user_id: "user-1".to_string(),
            edge_id: "transport-1".to_string(),
            edge_agent_id: "edge-1".to_string(),
            unresolved_request_ids: vec!["invocation-2".to_string(), "invocation-1".to_string()],
            replay_policy: EdgeHeartbeatReplayPolicy::DurableResultReconciliationRequired,
            ack_request_ids: Vec::new(),
        };

        assert!(ctx.reconciliation_changed(&unresolved));
        assert!(
            !ctx.reconciliation_changed(&EdgeHeartbeatResponse {
                unresolved_request_ids: vec![
                    "invocation-1".to_string(),
                    "invocation-2".to_string()
                ],
                ..unresolved.clone()
            }),
            "ordering-only differences must not amplify the same error"
        );

        let resolved = EdgeHeartbeatResponse {
            unresolved_request_ids: Vec::new(),
            ..unresolved.clone()
        };
        assert!(
            !ctx.reconciliation_changed(&resolved),
            "recovery clears diagnostics state without emitting an error"
        );
        assert!(
            ctx.reconciliation_changed(&unresolved),
            "a recurrence after recovery must be visible"
        );
    }

    #[test]
    #[serial]
    fn completed_request_ids_ring_buffer() {
        let ctx = edge_lifecycle();
        if let Ok(mut ids) = ctx.completed_request_ids.lock() {
            ids.clear();
        }
        for i in 0..300 {
            record_completed_request(format!("req-{i}"));
        }
        let snapshot = completed_request_ids_snapshot();
        assert_eq!(snapshot.len(), 256, "ring buffer caps at 256 entries");
        // Oldest entries evicted
        assert!(!snapshot.contains(&"req-0".to_string()));
        // Newest entries preserved
        assert!(snapshot.contains(&"req-299".to_string()));
    }

    #[test]
    #[serial]
    fn pending_request_guard_decrements_on_drop() {
        let ctx = edge_lifecycle();
        ctx.pending_tool_requests.store(0, Ordering::Relaxed);
        {
            let _guard = PendingToolRequestGuard::acquire();
            assert_eq!(ctx.pending_tool_requests.load(Ordering::Acquire), 1);
        }
        assert_eq!(ctx.pending_tool_requests.load(Ordering::Acquire), 0);
    }

    #[test]
    #[serial]
    fn enrich_register_body_persists_worktree_path() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let ctx = edge_lifecycle();
        if let Ok(mut guard) = ctx.registered_worktree_path.lock() {
            *guard = None;
        }

        let mut body = EdgeRegisterRequest::new("edge-test");
        body.worktree_path = Some(temp.path().display().to_string());
        enrich_register_body(&mut body);

        assert_eq!(
            ctx.registered_worktree_path(),
            Some(temp.path().to_path_buf())
        );
    }

    #[test]
    fn attach_runtime_environment_capabilities_uses_registered_worktree() {
        let mut body = EdgeRegisterRequest::new("edge-test");
        body.worktree_path = Some("/workspace/project".to_string());

        attach_runtime_environment_capabilities(&mut body);

        let capabilities = body.capabilities.expect("runtime capabilities");
        assert_eq!(capabilities["schema_version"], 1);
        assert_eq!(
            capabilities["binding"]["workspace"]["kind"],
            "edge_workspace"
        );
        assert_eq!(
            capabilities["binding"]["workspace"]["cwd"],
            "/workspace/project"
        );
        assert_eq!(
            capabilities["binding"]["executor"]["executor_id"],
            "edge-test"
        );
        assert_eq!(
            capabilities["binding"]["capabilities"]["runtime"]["runtime_has_shell"],
            true
        );
    }
}
