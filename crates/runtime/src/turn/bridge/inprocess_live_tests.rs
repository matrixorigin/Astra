//! Live MatrixOne coverage for the in-process streaming inference boundary.
//!
//! These tests deliberately cross the HTTP-provider and durable-database
//! boundaries. Unit tests in `llm_stream` cover protocol parsing; this suite
//! proves that the bridge admits one logical invocation before provider I/O,
//! records the physical attempt, and converges both records after success or
//! an abrupt client disconnect.

use super::*;

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{Router, body::Body, extract::State, response::Response as AxumResponse, routing::post};
use http_body_util::BodyExt;
use serial_test::serial;
use sqlx::Row;

const LIVE_DB_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct AwaitablePersistTracker {
    handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl crate::matrix_cloud_runtime::BridgePersistTracker for AwaitablePersistTracker {
    fn track_persist_task(&self, task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        self.handles
            .lock()
            .expect("persist tracker lock")
            .push(tokio::spawn(task));
    }
}

impl AwaitablePersistTracker {
    async fn drain(&self) {
        let handles = std::mem::take(&mut *self.handles.lock().expect("persist tracker lock"));
        for handle in handles {
            tokio::time::timeout(LIVE_DB_TIMEOUT, handle)
                .await
                .expect("bridge sidecar persistence timed out")
                .expect("bridge sidecar persistence panicked");
        }
    }
}

struct MockProvider {
    endpoint: String,
    task: tokio::task::JoinHandle<()>,
}

impl MockProvider {
    async fn stop(self) {
        self.task.abort();
        match self.task.await {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) => panic!("mock provider task failed: {error}"),
        }
    }
}

async fn spawn_success_provider() -> MockProvider {
    async fn handler() -> AxumResponse {
        let text = json!({
            "id": "resp-bridge-ledger",
            "choices": [{"delta": {"content": "durable bridge reply"}}]
        });
        let terminal = json!({
            "id": "resp-bridge-ledger",
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 4,
                "prompt_tokens_details": {"cached_tokens": 3}
            }
        });
        let body = format!("data: {text}\n\ndata: {terminal}\n\ndata: [DONE]\n\n");
        AxumResponse::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(body))
            .expect("valid mock provider response")
    }

    spawn_provider(Router::new().route("/chat/completions", post(handler))).await
}

#[derive(Clone)]
struct PendingProviderState {
    request_received: Arc<tokio::sync::Semaphore>,
}

async fn spawn_pending_provider() -> (MockProvider, Arc<tokio::sync::Semaphore>) {
    async fn handler(State(state): State<PendingProviderState>) -> AxumResponse {
        state.request_received.add_permits(1);
        let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        AxumResponse::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(pending))
            .expect("valid pending provider response")
    }

    let request_received = Arc::new(tokio::sync::Semaphore::new(0));
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(PendingProviderState {
            request_received: request_received.clone(),
        });
    (spawn_provider(app).await, request_received)
}

async fn spawn_provider(app: Router) -> MockProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock provider");
    let address = listener.local_addr().expect("mock provider address");
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock provider");
    });
    MockProvider {
        endpoint: format!("http://{address}/chat/completions"),
        task,
    }
}

fn live_test_encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(
        FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=")
            .expect("valid test encryption key"),
    )
}

fn admitted_server_execution(endpoint: &str) -> astra_services::AdmittedModelExecution {
    let mut execution = astra_services::AdmittedModelExecution::from_endpoint(
        "offer-live-bridge".to_string(),
        "mock-openai-model".to_string(),
        "openai".to_string(),
        endpoint.to_string(),
        "Bearer live-bridge-test".to_string(),
        Some(5_000),
    );
    execution.access_kind = astra_services::ModelAccessKind::SelfHosted;
    execution.execution_placement = astra_services::ModelExecutionPlacement::Server;
    execution
}

fn live_bridge_headers(user_id: &str, session_id: &str, query_event_id: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-mo-user-id", user_id.parse().expect("user header"));
    headers.insert(
        "x-mo-session-id",
        session_id.parse().expect("session header"),
    );
    headers.insert("x-mo-session-turn", "1".parse().expect("turn header"));
    headers.insert(
        "x-mo-user-query-event-id",
        query_event_id.parse().expect("query event header"),
    );
    headers.insert(
        "x-mo-turn-chain-id",
        format!("chain-{query_event_id}")
            .parse()
            .expect("turn chain header"),
    );
    headers.insert(
        ROOT_TURN_JOURNAL_HEADER,
        "1".parse().expect("journal owner header"),
    );
    headers
}

fn live_bridge_payload(prompt: &str) -> Bytes {
    Bytes::from(
        json!({
            "inference_purpose": astra_turn_types::InferencePurpose::PrimaryAgent,
            "root_turn_journal_owned": true,
            "messages": [{"role": "user", "content": prompt}],
            "edge_tools": [],
            "edge_profile": {}
        })
        .to_string(),
    )
}

async fn forward_live_bridge(
    bridge: &InProcessChatTurnBridge,
    headers: &HeaderMap,
    payload: Bytes,
    execution: astra_services::AdmittedModelExecution,
) -> Response {
    bridge
        .forward(
            headers,
            payload,
            execution,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(crate::InMemoryTurnReflectionStateStore::default()),
            Arc::new(crate::NoopTurnReflectionLessonWriter),
            Arc::new(crate::NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        )
        .await
        .expect("forward live bridge request")
}

async fn seed_session(pool: &SharedPool, user_id: &str, session_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, status, event_count, project_retention_policy,
          created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', 0, 'session', NOW(6), NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .expect("seed live bridge session");
}

async fn cleanup_session(pool: &SharedPool, user_id: &str, session_id: &str) {
    for statement in [
        "DELETE FROM prompt_deltas WHERE user_id = ? AND session_id = ?",
        "DELETE FROM prompt_request_records WHERE user_id = ? AND session_id = ?",
        "DELETE FROM agent_events WHERE user_id = ? AND session_id = ?",
        "DELETE FROM inference_invocation_settlement_debts WHERE user_id = ? AND session_id = ?",
        "DELETE FROM inference_provider_attempts WHERE user_id = ? AND session_id = ?",
        "DELETE FROM inference_invocations WHERE user_id = ? AND session_id = ?",
        "DELETE FROM inference_routes WHERE user_id = ? AND session_id = ?",
        "DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    ] {
        sqlx::query(statement)
            .bind(user_id)
            .bind(session_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("cleanup `{statement}`: {error}"));
    }
}

fn parse_sse_events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|block| block.trim().strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("bridge emitted valid JSON SSE"))
        .collect()
}

async fn wait_for_ledger_status(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    expected_invocation: &str,
    expected_attempt: &str,
) -> (String, String, Option<String>, Option<String>) {
    tokio::time::timeout(LIVE_DB_TIMEOUT, async {
        loop {
            let row = sqlx::query(
                "SELECT i.status AS invocation_status,
                        a.status AS attempt_status,
                        i.error_kind AS invocation_error_kind,
                        a.error_kind AS attempt_error_kind
                 FROM inference_invocations i
                 JOIN inference_provider_attempts a
                   ON a.user_id = i.user_id AND a.invocation_id = i.invocation_id
                 WHERE i.user_id = ? AND i.session_id = ? AND a.attempt_index = 0
                 LIMIT 1",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_optional(pool.get())
            .await
            .expect("read bridge inference ledger");
            if let Some(row) = row {
                let actual = (
                    row.get::<String, _>("invocation_status"),
                    row.get::<String, _>("attempt_status"),
                    row.get::<Option<String>, _>("invocation_error_kind"),
                    row.get::<Option<String>, _>("attempt_error_kind"),
                );
                if actual.0 == expected_invocation && actual.1 == expected_attempt {
                    return actual;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "ledger did not converge to invocation={expected_invocation}, attempt={expected_attempt}"
        )
    })
}

#[tokio::test]
#[ignore = "requires live MatrixOne: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn streaming_bridge_persists_one_success_with_exact_route_usage_and_provider_identity() {
    let pool = crate::turn::services::setup_live_pool_for_test().await;
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("bridge-user-{suffix}");
    let session_id = format!("bridge-session-{suffix}");
    let query_event_id = format!("bridge-query-{suffix}");
    seed_session(&pool, &user_id, &session_id).await;

    let provider = spawn_success_provider().await;
    let tracker = Arc::new(AwaitablePersistTracker::default());
    let bridge = InProcessChatTurnBridge::new(pool.settings().clone(), live_test_encryptor())
        .with_pool(pool.clone())
        .with_persist_tracker(tracker.clone());
    let response = forward_live_bridge(
        &bridge,
        &live_bridge_headers(&user_id, &session_id, &query_event_id),
        live_bridge_payload("prove successful durable inference"),
        admitted_server_execution(&provider.endpoint),
    )
    .await;
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect bridge response")
        .to_bytes();
    let events = parse_sse_events(std::str::from_utf8(&body).expect("UTF-8 bridge response"));
    assert!(events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("text_delta")
            && event.get("content").and_then(Value::as_str) == Some("durable bridge reply")
    }));
    assert!(
        events
            .iter()
            .any(|event| { event.get("type").and_then(Value::as_str) == Some("turn_complete") })
    );

    let rows = sqlx::query(
        "SELECT i.status, i.operation_id, i.purpose,
                i.input_tokens, i.output_tokens, i.cache_read_tokens,
                i.cache_creation_tokens, i.provider_response_id,
                r.offering_id, r.resolved_model_name, r.upstream_model_name,
                r.provider, r.execution_placement, r.access_kind,
                a.attempt_index, a.status AS attempt_status,
                a.input_tokens AS attempt_input_tokens,
                a.output_tokens AS attempt_output_tokens,
                a.cache_read_tokens AS attempt_cache_read_tokens,
                a.cache_creation_tokens AS attempt_cache_creation_tokens,
                a.provider_response_id AS attempt_provider_response_id
         FROM inference_invocations i
         JOIN inference_routes r
           ON r.user_id = i.user_id AND r.route_id = i.route_id
         JOIN inference_provider_attempts a
           ON a.user_id = i.user_id AND a.invocation_id = i.invocation_id
         WHERE i.user_id = ? AND i.session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_all(pool.get())
    .await
    .expect("load successful bridge inference ledger");
    assert_eq!(
        rows.len(),
        1,
        "one request must create one physical attempt"
    );
    let row = &rows[0];
    assert_eq!(row.get::<String, _>("status"), "succeeded");
    assert_eq!(row.get::<String, _>("attempt_status"), "succeeded");
    assert_eq!(
        row.get::<String, _>("operation_id"),
        bridge_inference_operation_id(&query_event_id)
    );
    assert_eq!(row.get::<String, _>("purpose"), "primary_agent");
    assert_eq!(row.get::<String, _>("offering_id"), "offer-live-bridge");
    assert_eq!(
        row.get::<String, _>("resolved_model_name"),
        "mock-openai-model"
    );
    assert_eq!(
        row.get::<String, _>("upstream_model_name"),
        "mock-openai-model"
    );
    assert_eq!(row.get::<String, _>("provider"), "openai");
    assert_eq!(row.get::<String, _>("execution_placement"), "server");
    assert_eq!(row.get::<String, _>("access_kind"), "self_hosted");
    assert_eq!(row.get::<i64, _>("attempt_index"), 0);
    for column in ["input_tokens", "attempt_input_tokens"] {
        assert_eq!(row.get::<i64, _>(column), 8, "column {column}");
    }
    for column in ["output_tokens", "attempt_output_tokens"] {
        assert_eq!(row.get::<i64, _>(column), 4, "column {column}");
    }
    for column in ["cache_read_tokens", "attempt_cache_read_tokens"] {
        assert_eq!(row.get::<i64, _>(column), 3, "column {column}");
    }
    for column in ["cache_creation_tokens", "attempt_cache_creation_tokens"] {
        assert_eq!(row.get::<i64, _>(column), 0, "column {column}");
    }
    assert_eq!(
        row.get::<i64, _>("input_tokens")
            + row.get::<i64, _>("cache_read_tokens")
            + row.get::<i64, _>("cache_creation_tokens"),
        11,
        "canonical fresh/read/write buckets must conserve provider prompt_tokens"
    );
    assert_eq!(
        row.get::<i64, _>("attempt_input_tokens")
            + row.get::<i64, _>("attempt_cache_read_tokens")
            + row.get::<i64, _>("attempt_cache_creation_tokens"),
        11,
        "physical-attempt buckets must conserve provider prompt_tokens"
    );
    assert_eq!(
        row.get::<Option<String>, _>("provider_response_id")
            .as_deref(),
        Some("resp-bridge-ledger")
    );
    assert_eq!(
        row.get::<Option<String>, _>("attempt_provider_response_id")
            .as_deref(),
        Some("resp-bridge-ledger")
    );

    tracker.drain().await;
    drop(bridge);
    provider.stop().await;
    cleanup_session(&pool, &user_id, &session_id).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn dropping_stream_after_provider_delivery_closes_attempt_and_invocation_as_unknown() {
    let pool = crate::turn::services::setup_live_pool_for_test().await;
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("bridge-drop-user-{suffix}");
    let session_id = format!("bridge-drop-session-{suffix}");
    let query_event_id = format!("bridge-drop-query-{suffix}");
    seed_session(&pool, &user_id, &session_id).await;

    let (provider, request_received) = spawn_pending_provider().await;
    let tracker = Arc::new(AwaitablePersistTracker::default());
    let bridge = InProcessChatTurnBridge::new(pool.settings().clone(), live_test_encryptor())
        .with_pool(pool.clone())
        .with_persist_tracker(tracker.clone());
    let response = forward_live_bridge(
        &bridge,
        &live_bridge_headers(&user_id, &session_id, &query_event_id),
        live_bridge_payload("disconnect after provider delivery"),
        admitted_server_execution(&provider.endpoint),
    )
    .await;
    let body_task = tokio::spawn(async move { response.into_body().collect().await });

    let permit = tokio::time::timeout(LIVE_DB_TIMEOUT, request_received.acquire())
        .await
        .expect("provider did not receive bridge request")
        .expect("provider request semaphore closed");
    permit.forget();
    wait_for_ledger_status(&pool, &user_id, &session_id, "admitted", "started").await;

    body_task.abort();
    let join_error = body_task
        .await
        .expect_err("aborted response collector must not finish normally");
    assert!(join_error.is_cancelled());

    let (_, _, invocation_error_kind, attempt_error_kind) = wait_for_ledger_status(
        &pool,
        &user_id,
        &session_id,
        "delivery_unknown",
        "delivery_unknown",
    )
    .await;
    assert_eq!(invocation_error_kind.as_deref(), Some("stream_transport"));
    assert_eq!(attempt_error_kind.as_deref(), Some("stream_transport"));

    let row = sqlx::query(
        "SELECT i.provider_response_id, i.input_tokens, i.output_tokens,
                a.provider_response_id AS attempt_provider_response_id,
                a.input_tokens AS attempt_input_tokens,
                a.output_tokens AS attempt_output_tokens
         FROM inference_invocations i
         JOIN inference_provider_attempts a
           ON a.user_id = i.user_id AND a.invocation_id = i.invocation_id
         WHERE i.user_id = ? AND i.session_id = ? AND a.attempt_index = 0",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("load disconnected bridge terminal facts");
    assert_eq!(row.get::<Option<String>, _>("provider_response_id"), None);
    assert_eq!(
        row.get::<Option<String>, _>("attempt_provider_response_id"),
        None
    );
    assert_eq!(row.get::<i64, _>("input_tokens"), 0);
    assert_eq!(row.get::<i64, _>("output_tokens"), 0);
    assert_eq!(row.get::<i64, _>("attempt_input_tokens"), 0);
    assert_eq!(row.get::<i64, _>("attempt_output_tokens"), 0);

    tracker.drain().await;
    drop(bridge);
    provider.stop().await;
    cleanup_session(&pool, &user_id, &session_id).await;
    pool.close().await;
}
