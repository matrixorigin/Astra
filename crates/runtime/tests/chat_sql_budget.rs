//! Public chat persistence budget, including work after the response ends.
#![cfg(feature = "e2e-hooks")]

#[allow(dead_code, unused_imports)]
#[path = "system_matrix_http_e2e/harness.rs"]
mod harness;
mod test_support;

use harness::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tracing_subscriber::prelude::*;

/// The run drain does not own the detached observer. Its completion counter is
/// published only after the production worker has finished all of its SQL.
async fn wait_for_chat_observers(
    registry: Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>,
    expected_observers: u64,
) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let metrics = registry.render_prometheus();
            let mut completed = 0;
            let mut scheduled = 0;
            for line in metrics.lines().filter(|line| {
                line.starts_with("astra_turn_observer_runs_total{")
                    || line.starts_with("astra_turn_observer_dispatches_total{")
            }) {
                let (series, value) = line.rsplit_once(' ').unwrap();
                let value: u64 = value.parse().unwrap();
                if value == 0 {
                    continue;
                }
                if series.starts_with("astra_turn_observer_runs_total{") {
                    assert_eq!(
                        series,
                        "astra_turn_observer_runs_total{mode=\"async\",outcome=\"success\"}",
                        "authorized SQL budget requires successful observation, not skipped or failed work: {line}"
                    );
                    completed += value;
                } else {
                    assert_eq!(
                        series,
                        "astra_turn_observer_dispatches_total{mode=\"async\",outcome=\"scheduled\"}",
                        "authorized SQL budget requires scheduled observation: {line}"
                    );
                    scheduled += value;
                }
            }
            assert!(
                scheduled <= expected_observers,
                "unexpected extra scheduled observer"
            );
            assert!(completed <= expected_observers, "unexpected extra observer");
            if scheduled == expected_observers && completed == expected_observers {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("each chat must finish its detached observer before ending SQL capture");
}

fn observer_metrics(
    scheduled: u64,
    successes: u64,
) -> Arc<astra_turn_core::pipeline_metrics::MetricsRegistry> {
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
    registry.register_counter("astra_turn_observer_dispatches_total", "observer test");
    registry.register_counter("astra_turn_observer_runs_total", "observer test");
    registry.increment_counter(
        "astra_turn_observer_dispatches_total",
        &[("mode", "async"), ("outcome", "scheduled")],
        scheduled,
    );
    registry.increment_counter(
        "astra_turn_observer_runs_total",
        &[("mode", "async"), ("outcome", "success")],
        successes,
    );
    registry
}

#[tokio::test]
async fn sql_budget_requires_the_expected_successful_observations() {
    wait_for_chat_observers(observer_metrics(2, 2), 2).await;
}

#[tokio::test]
async fn sql_budget_rejects_extra_scheduled_but_pending_observer() {
    let result = tokio::spawn(wait_for_chat_observers(observer_metrics(2, 1), 1)).await;
    assert!(
        result.is_err_and(|error| error.is_panic()),
        "the first success must not end capture while a duplicate scheduled observer can still issue SQL"
    );
}

#[tokio::test]
async fn sql_budget_rejects_skipped_failed_or_dropped_observations() {
    for (metric, outcome) in [
        ("astra_turn_observer_runs_total", "not_configured"),
        ("astra_turn_observer_runs_total", "access_disabled"),
        ("astra_turn_observer_runs_total", "empty"),
        ("astra_turn_observer_runs_total", "error"),
        ("astra_turn_observer_dispatches_total", "dropped_full"),
        ("astra_turn_observer_dispatches_total", "skipped_empty"),
    ] {
        for successes in [0, 1] {
            let registry = observer_metrics(1, successes);
            registry.increment_counter(metric, &[("mode", "async"), ("outcome", outcome)], 1);
            let result = tokio::spawn(wait_for_chat_observers(registry, 1)).await;
            assert!(
                result.is_err_and(|error| error.is_panic()),
                "{outcome} must fail the authorized probe, including with {successes} successful observations"
            );
        }
    }
}

/// Measure request SQL on an initialized pool, not timing-dependent SQLx
/// connection setup. Every statement after this boundary still counts.
async fn prime_chat_pool(ctx: &MatrixE2eCtx) {
    let pool = ctx.shared_pool.get();
    // This serial probe needs only a small warm reserve, not the deployment's
    // full connection limit. Additional connections opened during a request
    // remain visible to the raw SQL budget.
    let reserve = pool.options().get_max_connections().min(4);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut connections = Vec::new();
        for _ in 0..reserve {
            connections.push(pool.acquire().await.unwrap());
        }
        drop(connections);
        while pool.num_idle() < reserve as usize {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initialize and return the chat-pool reserve before SQL capture");
}

#[derive(Default)]
struct Statement(String, f64);

impl tracing::field::Visit for Statement {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "db.statement" && !value.trim().is_empty() {
            self.0 = value.split_whitespace().collect::<Vec<_>>().join(" ");
        } else if field.name() == "summary" && self.0.is_empty() {
            self.0 = value.to_owned();
        }
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if field.name() == "elapsed_secs" {
            self.1 = value;
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

struct ObservedSql {
    sql: String,
    elapsed: std::time::Duration,
    finished_at: std::time::Instant,
}

#[derive(Clone, Default)]
struct SqlCapture(Arc<Mutex<Option<Vec<ObservedSql>>>>);

// This ordinary-chat probe owns sequential opening/terminal transactions.
// If a scenario introduces overlapping transactions, report the ambiguity
// instead of attributing concurrent connection events to the wrong owner.
fn transaction_windows(samples: &[ObservedSql]) -> Value {
    let mut active = None;
    let mut windows = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        let started_at = sample.finished_at - sample.elapsed;
        if sample.sql == "BEGIN" {
            if active.is_some() {
                return json!({"overlapping_transactions":true});
            }
            active = Some((index, started_at, None));
        }
        if let Some((_, _, ref mut lock_start)) = active
            && lock_start.is_none()
            && sample.sql.contains("FOR UPDATE")
        {
            // Includes waiting to acquire the first lock, therefore an
            // upper bound on holding that transaction's first lock.
            *lock_start = Some(started_at);
        }
        if matches!(sample.sql.as_str(), "COMMIT" | "ROLLBACK") {
            let Some((first, tx_start, lock_start)) = active.take() else {
                return json!({"unmatched_transaction_boundary":true});
            };
            let sql_ms = samples[first..=index]
                .iter()
                .map(|sample| sample.elapsed.as_secs_f64() * 1000.0)
                .sum::<f64>();
            let total_ms = sample.finished_at.duration_since(tx_start).as_secs_f64() * 1000.0;
            windows.push(json!({
                "statement_count": index - first + 1,
                "transaction_ms": total_ms,
                "sql_ms": sql_ms,
                "between_statements_ms": (total_ms - sql_ms).max(0.0),
                "lock_wait_and_hold_upper_bound_ms": lock_start.map(|start| sample.finished_at.duration_since(start).as_secs_f64() * 1000.0),
            }));
        }
    }
    json!({"windows":windows,"open_transaction":active.is_some()})
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SqlCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut guard = self.0.lock().unwrap();
        if let Some(statements) = guard.as_mut() {
            let mut statement = Statement::default();
            event.record(&mut statement);
            statements.push(ObservedSql {
                sql: statement.0,
                elapsed: std::time::Duration::from_secs_f64(statement.1),
                finished_at: std::time::Instant::now(),
            });
        }
    }
}

const CONTROL_OBSERVATION_SQL: &str = "SELECT requested.ordinal, run_id, session_id, status, waiting_for, parent_run_id, ancestor_path, run_generation, last_event_idx, CAST(cancellation_requested_at IS NOT NULL AS SIGNED) AS cancellation_requested, CAST(owner_pod_id IS NOT NULL AND owner_lease_expires_at IS NOT NULL AND owner_lease_expires_at >= NOW(6) AS SIGNED) AS owner_lease_live FROM (VALUES ROW(0,?,?)) requested(ordinal, requested_user_id, requested_run_id) LEFT JOIN agent_runs ON user_id = requested_user_id AND run_id = requested_run_id AND run_id IN (?)";

// The account/session snapshot is cached for 30 seconds. A slow fixture can
// cross that boundary independently of readonly tool rounds. Attribute only
// this exact query, at most once per request, while retaining the raw total.
const AUTHENTICATION_SNAPSHOT_SQL: &str = "SELECT u.user_id, u.username, u.email, u.display_name, u.is_active, CAST(UNIX_TIMESTAMP(s.session_expires_at) AS SIGNED) AS session_expires_at_unix, m.provider_id AS memoria_provider_id, m.external_subject AS memoria_subject FROM (SELECT MAX(expires_at) AS session_expires_at FROM auth_refresh_tokens WHERE session_id = ? AND user_id = ? AND is_revoked = 0 AND expires_at > NOW()) s LEFT JOIN auth_users u ON u.user_id = ? LEFT JOIN (SELECT astra_user_id, provider_id, external_subject FROM auth_external_identities WHERE astra_user_id = ? AND provider_id LIKE 'memoria:%' ORDER BY CASE WHEN provider_id = ? THEN 0 ELSE 1 END, updated_at DESC, provider_id ASC LIMIT 1) m ON m.astra_user_id = u.user_id";

fn authentication_snapshot_count(statements: &[String]) -> usize {
    let count = statements
        .iter()
        .filter(|sql| sql.as_str() == AUTHENTICATION_SNAPSHOT_SQL)
        .count();
    assert!(
        count <= 1,
        "a request must not repeat its authentication snapshot"
    );
    count
}

fn answer(message: Value, reason: &str) -> ProviderResponse {
    ProviderResponse::OpenAi(json!({
        "choices": [{"index": 0, "message": message, "finish_reason": reason}],
        "usage": {"prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49}
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn unavailable_sessions_reject_both_chat_entrypoints_before_execution() {
    let b = bootstrap().await;
    let model = format!("mock-{}", b.ctx.suffix);
    b.ctx
        .install_native_provider(
            &b.auth_header,
            vec![ProviderScript::new(
                "no provider request is admitted",
                move |request| request.body["model"] == model,
                Vec::new(),
            )],
        )
        .await;
    let missing = uuid::Uuid::new_v4().to_string();
    let foreign = uuid::Uuid::new_v4().to_string();
    let closed = uuid::Uuid::new_v4().to_string();
    let deleting = uuid::Uuid::new_v4().to_string();
    for (session, owner, status) in [
        (&foreign, "unrelated-owner", "active"),
        (&closed, b.ctx.user_id.as_str(), "closed"),
        (&deleting, b.ctx.user_id.as_str(), "deleting"),
    ] {
        sqlx::query("INSERT INTO agent_sessions (session_id, user_id, status) VALUES (?, ?, ?)")
            .bind(session)
            .bind(owner)
            .bind(status)
            .execute(&b.ctx.pool)
            .await
            .unwrap();
    }
    for session in [&missing, &foreign, &closed, &deleting] {
        let payload = json!({"message":"must not execute", "session_id":session,
            "model_selection":seeded_model_selection(&b.ctx)});
        let (status, error) =
            post_json(&b.ctx.app, "/chat", Some(&b.auth_header), payload.clone()).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{error}");
        assert_eq!(error["error_code"], "session_unavailable", "{error}");
        assert_eq!(error["metadata"]["admission_state"], "rejected", "{error}");
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/chat/stream")
            .header("authorization", &b.auth_header)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
        let (_, body) = collect_full_sse_stream(&b.ctx.app, request, 5).await;
        let events = parse_sse_events(&body);
        let error = events
            .iter()
            .find(|event| event["type"] == "error")
            .expect(&body);
        assert_eq!(error["error_code"], "session_unavailable", "{body}");
        assert_eq!(error["metadata"]["admission_state"], "rejected", "{body}");
        assert!(
            !events.iter().any(|event| event["type"] == "run_started"),
            "{body}"
        );
    }
    assert!(b.ctx.native_provider_requests().await.is_empty());
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id = ?")
        .bind(&b.ctx.user_id)
        .fetch_one(&b.ctx.pool)
        .await
        .unwrap();
    assert_eq!(
        runs, 0,
        "rejection must not create an executable or failed Run"
    );
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    b.ctx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn ordinary_chat_releases_session_locks_before_waiting_for_the_provider() {
    use tokio::sync::Notify;
    let b = bootstrap().await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let signal = entered.clone();
    let model = format!("mock-{}", b.ctx.suffix);
    let response = format!("data: {}\n\n", json!({
        "id":"held-provider", "object":"chat.completion.chunk", "model":model,
        "choices":[{"index":0,"delta":{"role":"assistant","content":"Locks were released."},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}
    })).into_bytes();
    b.ctx
        .install_native_provider(
            &b.auth_header,
            vec![ProviderScript::new(
                "provider wait after committed opening",
                move |request| {
                    let matches = request.body["model"] == model && request.body["stream"] == true;
                    if matches {
                        signal.notify_one();
                    }
                    matches
                },
                vec![ProviderResponse::Stream {
                    content_type: "text/event-stream",
                    chunks: vec![response, b"data: [DONE]\n\n".to_vec()],
                    release_before_chunk: Some((0, release.clone())),
                }],
            )],
        )
        .await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", &b.auth_header)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({"message":"hi", "session_id":b.ctx.session_id,
            "model_selection":seeded_model_selection(&b.ctx)})
            .to_string(),
        ))
        .unwrap();
    let app = b.ctx.app.clone();
    let chat = tokio::spawn(async move { collect_full_sse_stream(&app, request, 30).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    // An independent connection must be able to lock both admission owners
    // while the provider response is deliberately blocked. Close on drop also
    // makes a failed timeout unable to return a busy connection to the pool.
    let mut connection = b.ctx.pool.acquire().await.unwrap();
    connection.close_on_drop();
    let available = tokio::time::timeout(std::time::Duration::from_secs(2), sqlx::query(
        "SELECT session.status FROM agent_session_lifecycle_fences fence
         JOIN agent_sessions session ON session.user_id = fence.user_id AND session.session_id = fence.session_id
         WHERE fence.user_id = ? AND fence.session_id = ? FOR UPDATE",
    ).bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&mut *connection)).await;
    drop(connection);
    release.notify_one();
    let (status, body) = chat.await.unwrap();
    assert!(
        available.is_ok(),
        "a Session transaction spanned the provider wait"
    );
    available.unwrap().unwrap();
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert!(
        parse_sse_events(&body)
            .iter()
            .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
        "{body}"
    );
    assert!(body.contains("Locks were released."), "{body}");
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    b.ctx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; provider is scripted localhost HTTP"]
async fn ordinary_chat_and_readonly_rounds_have_bounded_total_sql() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .expect("this integration binary owns its SQL subscriber");

    let mut budgets = Vec::new();
    for (scenario, readonly_rounds, warm_context, held_for_control) in [
        ("ordinary", 0, false, false),
        ("discovery", 1, false, false),
        ("discovery", 3, false, false),
        ("agent_list", 1, false, false),
        ("agent_list", 3, false, false),
        ("ordinary", 0, true, false),
        ("agent_list", 3, true, false),
        ("ordinary", 0, false, true),
        ("agent_list", 3, false, true),
    ] {
        let executes_agent = scenario == "agent_list";
        let tool_call_count = readonly_rounds + usize::from(executes_agent);
        let b = bootstrap().await;
        let model = format!("mock-{}", b.ctx.suffix);
        // Exercise the non-null deployment branch of the actual UNION query.
        sqlx::query(
            "UPDATE infra_llm_models SET max_completion_tokens = 1024 WHERE model_name = ?",
        )
        .bind(&model)
        .execute(&b.ctx.pool)
        .await
        .unwrap();
        let mut responses = (0..readonly_rounds)
            .map(|round| {
                answer(
                    json!({"role": "assistant", "content": "", "tool_calls": [{
                        "id": format!("discover-{round}"), "type": "function",
                        "function": if executes_agent {
                            json!({"name":"invoke_tool", "arguments":json!({"name":"agent","arguments":{"action":"list"}}).to_string()})
                        } else {
                            json!({"name":"tool_search", "arguments":"{\"query\":\"select:agent\"}"})
                        }
                    }]}),
                    "tool_calls",
                )
            })
            .collect::<Vec<_>>();
        if executes_agent {
            responses.insert(
                0,
                answer(
                    json!({"role":"assistant","content":"", "tool_calls":[{
                        "id":"select-agent", "type":"function",
                        "function":{"name":"tool_search","arguments":"{\"query\":\"select:agent\"}"}
                    }]}),
                    "tool_calls",
                ),
            );
        }
        responses.push(answer(
            json!({"role": "assistant", "content": "Discovery complete."}),
            "stop",
        ));
        if warm_context {
            responses.insert(
                0,
                answer(
                    json!({"role":"assistant", "content":"Warm context ready."}),
                    "stop",
                ),
            );
        }
        let release = Arc::new(tokio::sync::Notify::new());
        if held_for_control {
            let ProviderResponse::OpenAi(first) = responses.remove(0) else {
                panic!("controlled scenario starts with the ordinary provider response");
            };
            let mut delta = first["choices"][0]["message"].clone();
            if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for (index, call) in calls.iter_mut().enumerate() {
                    call["index"] = json!(index);
                }
            }
            let chunk = format!(
                "data: {}\n\n",
                json!({
                    "id":"control-observation", "object":"chat.completion.chunk", "model":model,
                    "choices":[{"index":0,"delta":delta,
                        "finish_reason":first["choices"][0]["finish_reason"]}],
                    "usage":first["usage"]
                })
            )
            .into_bytes();
            responses.insert(
                0,
                ProviderResponse::Stream {
                    content_type: "text/event-stream",
                    chunks: vec![chunk, b"data: [DONE]\n\n".to_vec()],
                    release_before_chunk: Some((0, release.clone())),
                },
            );
        }
        b.ctx
            .install_native_provider(
                &b.auth_header,
                vec![ProviderScript::new(
                    "public ordinary conversation and readonly discovery",
                    move |request| request.body["model"] == model && request.body["stream"] == true,
                    responses,
                )],
            )
            .await;
        // Raw history can contain an imported row without a canonical head.
        // It stays visible to history readers, never an alternate prompt source.
        let imported = "Imported transcript is not canonical model context.";
        let mut hash = Sha256::new();
        hash.update(b"assistant\0");
        hash.update(imported.as_bytes());
        hash.update([0]);
        sqlx::query(
            "INSERT INTO session_transcript_items (session_id, item_seq, user_id, role, content, source_event_id, content_hash, created_at) VALUES (?, 1, ?, 'assistant', ?, 'imported-history', ?, NOW(6))",
        ).bind(&b.ctx.session_id).bind(&b.ctx.user_id).bind(imported)
            .bind(format!("{:x}", hash.finalize())).execute(&b.ctx.pool).await.unwrap();
        if warm_context {
            let warm_request = axum::http::Request::builder()
                .method("POST")
                .uri("/chat/stream")
                .header("authorization", &b.auth_header)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "message":"Remember the first turn.", "session_id":b.ctx.session_id,
                        "model_selection":seeded_model_selection(&b.ctx)
                    })
                    .to_string(),
                ))
                .unwrap();
            let (status, body) = collect_full_sse_stream(&b.ctx.app, warm_request, 30).await;
            assert_eq!(status, axum::http::StatusCode::OK, "{body}");
            assert!(
                parse_sse_events(&body)
                    .iter()
                    .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
                "{body}"
            );
            assert!(body.contains("Warm context ready."), "{body}");
            // Shutdown drain would reject the measured turn. The complete SSE
            // response settles the warm-up turn; separately await its observer.
            wait_for_chat_observers(b.ctx.app_state.metrics_registry(), 1).await;
        }
        prime_chat_pool(&b.ctx).await;
        let message = if executes_agent {
            "List agent status."
        } else if readonly_rounds == 0 {
            "hi"
        } else {
            "Discover the agent tool without invoking it."
        };
        let execution_started = std::time::Instant::now();
        *capture.0.lock().unwrap() = Some(Vec::new());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/chat/stream")
            .header("authorization", &b.auth_header)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({
                    "message": message,
                    "session_id": b.ctx.session_id,
                    "model_selection": seeded_model_selection(&b.ctx)
                })
                .to_string(),
            ))
            .unwrap();
        let observe_control = async {
            if held_for_control {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        let observed = capture.0.lock().unwrap().as_ref().is_some_and(|samples| {
                            samples
                                .iter()
                                .any(|sample| sample.sql == CONTROL_OBSERVATION_SQL)
                        });
                        if observed {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("held provider must observe the real durable control poll");
                release.notify_one();
            }
        };
        let ((status, body), ()) = tokio::join!(
            collect_full_sse_stream(&b.ctx.app, request, 30),
            observe_control,
        );
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let response_finished_at = std::time::Instant::now();
        let events = parse_sse_events(&body);
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
            "{body}"
        );
        assert!(body.contains("Discovery complete."), "{body}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "tool_call_end")
                .count(),
            tool_call_count,
            "tool outcomes must be delivered exactly once: {body}"
        );
        if executes_agent {
            let endings = events
                .iter()
                .filter(|event| event["type"] == "tool_call_end" && event["tool"] == "agent")
                .collect::<Vec<_>>();
            assert_eq!(
                endings.len(),
                readonly_rounds,
                "each real handler call needs its own ending: {body}"
            );
            assert!(
                endings
                    .iter()
                    .all(|event| event["success"] == true || event["status"] == "completed"),
                "every actual readonly call must succeed, not merely be discovered or skipped: {body}"
            );
        }
        assert!(
            !events.iter().any(|event| event["type"] == "error"),
            "{body}"
        );
        assert!(
            b.ctx
                .app_state
                .drain_background_runs(std::time::Duration::from_secs(5))
                .await
        );
        wait_for_chat_observers(
            b.ctx.app_state.metrics_registry(),
            1 + u64::from(warm_context),
        )
        .await;
        let samples = capture.0.lock().unwrap().take().unwrap();
        let execution_ms = execution_started.elapsed().as_secs_f64() * 1000.0;
        // Explicit cold-replay verification follows the ordinary request's
        // complete drain and stopped SQL capture; it is not ordinary execution.
        let run_id = events
            .iter()
            .find(|event| event["type"] == "session_info")
            .and_then(|event| event["run_id"].as_str())
            .unwrap();
        let receipt: String = sqlx::query_scalar(
            "SELECT terminal_receipt_json FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(&b.ctx.user_id)
        .bind(run_id)
        .fetch_one(&b.ctx.pool)
        .await
        .unwrap();
        let receipt: astra_services::runs::CommittedRunTerminalReceipt =
            serde_json::from_str(&receipt).unwrap();
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(format!(
                "/chat/runs/{run_id}/stream?last_index=0&replay_only=true"
            ))
            .header("authorization", &b.auth_header)
            .body(axum::body::Body::empty())
            .unwrap();
        let (status, replay) = collect_full_sse_stream(&b.ctx.app, request, 30).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{replay}");
        let replay = parse_sse_events(&replay);
        let mut accumulated = astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
        for event in &replay {
            astra_turn_core::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
                &format!("data: {}\n\n", serde_json::to_string(event).unwrap()),
                &mut accumulated,
                &mut Vec::new(),
            );
        }
        assert!(
            replay
                .iter()
                .any(|event| event["type"] == "usage" && event["usage_scope"] == "run_total")
        );
        assert_eq!(accumulated.qualified_usage, receipt.receipt.usage);
        assert!(accumulated.current_request_usage.is_none());

        let control_observation_sql = samples
            .iter()
            .filter(|sample| sample.sql == CONTROL_OBSERVATION_SQL)
            .count();
        if held_for_control {
            assert!(control_observation_sql > 0);
        }

        let transactions = transaction_windows(&samples);
        let after_response_sql = samples
            .iter()
            .filter(|sample| sample.finished_at > response_finished_at)
            .count();
        let statements = samples
            .iter()
            .map(|sample| sample.sql.clone())
            .collect::<Vec<_>>();
        for sql in &statements {
            assert!(
                !sql.starts_with("SELECT requested.payload_json AS requested_payload")
                    && !sql.starts_with("SELECT last_event_idx FROM agent_runs")
                    && !(sql.starts_with("SELECT")
                        && sql.contains("FROM agent_run_events")
                        && sql.contains("UNION ALL")),
                "an unchanged opening control tail must not load permission or input history: {sql}"
            );
        }
        assert!(
            !statements
                .iter()
                .any(|sql| sql.contains("FROM conversation_log")),
            "ordinary chat must use its admitted canonical history, without legacy CSL probes"
        );
        assert!(
            !statements
                .iter()
                .any(|sql| sql.contains("run_display_projections")),
            "ordinary chat must not maintain a separate display store"
        );
        let diagnostic_rows: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ?",
        )
        .bind(&b.ctx.user_id)
        .bind(&b.ctx.session_id)
        .fetch_one(&b.ctx.pool)
        .await
        .unwrap();
        assert_eq!(
            diagnostic_rows.0, 0,
            "ordinary chat must not persist optional diagnostic copies"
        );
        let (history_status, history) = get_json(
            &b.ctx.app,
            &format!(
                "/sessions/{}/transcript?scope=root_conversation&limit=20",
                b.ctx.session_id
            ),
            Some(b.auth_header.as_str()),
            &[],
        )
        .await;
        assert_eq!(history_status, axum::http::StatusCode::OK, "{history}");
        let items = history["items"]
            .as_array()
            .expect("refreshed history items");
        assert!(
            items
                .iter()
                .any(|item| item["role"] == "user" && item["content"] == message),
            "{history}"
        );
        assert!(items.iter().any(|item| item["role"] == "assistant" && item["content"] == "Discovery complete."), "{history}");
        assert!(
            statements
                .iter()
                .all(|sql| !sql.contains("resource_usage") && !sql.contains("resource_limits")),
            "ordinary Run usage belongs to its canonical terminal, not a synchronous daily quota mirror"
        );
        assert!(
            !statements.is_empty(),
            "the SQL observer must capture real database work"
        );
        let requests = b.ctx.native_provider_requests().await;
        assert!(requests.iter().all(|request| {
            request.body["max_completion_tokens"]
                .as_u64()
                .is_some_and(|limit| limit > 0 && limit <= 1024)
        }));
        assert_eq!(requests[0].body["max_completion_tokens"], 1024);
        assert_eq!(
            requests.len(),
            tool_call_count + 1 + usize::from(warm_context)
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.body.to_string().contains(imported)),
            "raw transcript without canonical Context must not be injected into provider requests"
        );
        if warm_context {
            assert!(
                requests.last().unwrap().body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "assistant"
                        && message["content"] == "Warm context ready."),
                "next turn must consume the committed canonical history"
            );
        }
        if readonly_rounds > 0 {
            assert!(
                requests.last().unwrap().body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool")
            );
        }
        if scenario == "ordinary" && !warm_context && !held_for_control {
            use astra_services::session_restore::{HybridRestoreService, SessionRestoreService};
            let restore = HybridRestoreService::new(b.ctx.pool.clone());
            let extra_ids = (0..19)
                .map(|_| uuid::Uuid::new_v4().to_string())
                .collect::<Vec<_>>();
            for expected in [1, 20] {
                if expected == 20 {
                    let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
                        "INSERT INTO agent_sessions (user_id, session_id, status) ",
                    );
                    insert.push_values(&extra_ids, |mut row, id| {
                        row.push_bind(&b.ctx.user_id)
                            .push_bind(id)
                            .push_bind("active");
                    });
                    insert.build().execute(&b.ctx.pool).await.unwrap();
                }
                *capture.0.lock().unwrap() = Some(Vec::new());
                let directory = restore
                    .list_resumable_sessions(&b.ctx.user_id)
                    .await
                    .unwrap();
                let observed = capture.0.lock().unwrap().take().unwrap();
                assert_eq!(directory.len(), expected);
                assert_eq!(
                    observed.len(),
                    1,
                    "directory hydration must not grow with its row count"
                );
                for forbidden in [
                    "agent_events",
                    "session_artifacts",
                    "session_checkpoints",
                    "conversation_manifest_nodes",
                ] {
                    assert!(!observed[0].sql.contains(forbidden), "{}", observed[0].sql);
                }
                eprintln!("resumable_directory rows={expected} sql={}", observed.len());
            }
            let mut cleanup = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "DELETE FROM agent_sessions WHERE user_id = ",
            );
            cleanup
                .push_bind(&b.ctx.user_id)
                .push(" AND session_id IN (");
            let mut ids = cleanup.separated(", ");
            for id in &extra_ids {
                ids.push_bind(id);
            }
            ids.push_unseparated(")");
            cleanup.build().execute(&b.ctx.pool).await.unwrap();
        }
        b.ctx.close().await;
        eprintln!(
            "chat_sql_budget scenario={scenario} warm_context={warm_context} readonly_rounds={readonly_rounds} tool_calls={tool_call_count} held_for_control={held_for_control} control_polls={control_observation_sql} total_sql={}",
            statements.len()
        );
        if let Some(path) = std::env::var_os("ASTRA_TEST_SQL_BUDGET_REPORT") {
            let statement_timings = samples
                .iter()
                .map(|sample| {
                    json!({
                        "elapsed_ms": sample.elapsed.as_secs_f64() * 1000.0,
                        "sql": sample.sql,
                    })
                })
                .collect::<Vec<_>>();
            let record = json!({"scenario":scenario, "warm_context":warm_context, "readonly_rounds": readonly_rounds, "tool_calls":tool_call_count, "total_sql": statements.len(), "control_observation_sql":control_observation_sql, "held_for_control":held_for_control, "execution_ms":execution_ms, "statements": statements, "statement_timings":statement_timings, "transactions":transactions, "after_response_sql":after_response_sql});
            use std::io::Write as _;
            let mut report = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(report, "{record}").unwrap();
        }
        budgets.push((
            scenario,
            readonly_rounds,
            warm_context,
            control_observation_sql,
            statements,
        ));
    }
    for (scenario, rounds, warm_context, polls, statements) in &budgets {
        // Keep every query in the total budget, including periodic controls.
        assert!(
            statements.len() <= 20,
            "{scenario} (warm={warm_context}, polls={polls}) used {} SQL statements including transactions and drained work:\n{}",
            statements.len(),
            statements.join("\n")
        );
        let authentication_reads = authentication_snapshot_count(statements);
        if *scenario == "ordinary" {
            continue;
        }
        let baseline = budgets
            .iter()
            .find(|(name, _, warm, _, _)| *name == "ordinary" && warm == warm_context)
            .unwrap();
        // Explain exact periodic reads and at most one request-auth cache
        // miss. The independent raw total budget above never deducts a query.
        let expected = baseline.4.len() as i64 + *polls as i64 - baseline.3 as i64
            + authentication_reads as i64
            - authentication_snapshot_count(&baseline.4) as i64;
        assert_eq!(
            statements.len() as i64,
            expected,
            "{scenario} {rounds} readonly rounds (warm={warm_context}, polls={polls}) must add no SQL beyond exact periodic controls and one request-auth cache miss"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; provider is scripted localhost HTTP"]
async fn explicit_explain_keeps_diagnostic_ingestion_after_complete_drain() {
    require_system_e2e_env();
    let b = bootstrap().await;
    let model = format!("mock-{}", b.ctx.suffix);
    b.ctx
        .install_native_provider(
            &b.auth_header,
            vec![ProviderScript::new(
                "explicit diagnostic recording",
                move |request| request.body["model"] == model && request.body["stream"] == true,
                vec![answer(
                    json!({"role": "assistant", "content": "Observed."}),
                    "stop",
                )],
            )],
        )
        .await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", &b.auth_header)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({
                "message": "Reply briefly.",
                "session_id": b.ctx.session_id,
                "model_selection": seeded_model_selection(&b.ctx),
                "explain": true
            })
            .to_string(),
        ))
        .unwrap();
    let (status, body) = collect_full_sse_stream(&b.ctx.app, request, 30).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert!(
        parse_sse_events(&body)
            .iter()
            .any(|event| { event["type"] == "run_finished" && event["status"] == "completed" }),
        "{body}"
    );
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    let recorded: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type IN ('trace_span', 'llm_round')",
    )
    .bind(&b.ctx.user_id)
    .bind(&b.ctx.session_id)
    .fetch_one(&b.ctx.pool)
    .await
    .unwrap();
    assert!(recorded.0 > 0, "explicit diagnostics must remain available");
    assert_eq!(b.ctx.native_provider_requests().await.len(), 1);
    b.ctx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn post_provider_context_limit_preserves_answer_usage_and_tool_facts() {
    use astra_services::runs::{
        CanonicalTerminalContext, CanonicalTerminalOutcome, CommittedRunTerminalReceipt,
    };
    use sqlx::Row;

    let b = bootstrap().await;
    let model = format!("mock-{}", b.ctx.suffix);
    // This fits the provider's existing 16 MiB stream budget but exceeds the
    // canonical segment's 8 MiB once message framing is included.
    let large_answer = "x".repeat(8 * 1024 * 1024);
    let expected_hash = Sha256::digest(large_answer.as_bytes());
    b.ctx.install_native_provider(&b.auth_header, vec![ProviderScript::new(
        "real readonly tool followed by a large answer and one later turn",
        move |request| request.body["model"] == model && request.body["stream"] == true,
        vec![
            answer(json!({"role":"assistant", "content":"", "tool_calls":[{
                "id":"select-agent", "type":"function",
                "function":{"name":"tool_search", "arguments":"{\"query\":\"select:agent\"}"}
            }]}), "tool_calls"),
            answer(json!({"role":"assistant", "content":"", "tool_calls":[{
                "id":"list-agent", "type":"function",
                "function":{"name":"invoke_tool", "arguments":json!({"name":"agent","arguments":{"action":"list"}}).to_string()}
            }]}), "tool_calls"),
            answer(json!({"role":"assistant", "content":large_answer}), "stop"),
            answer(json!({"role":"assistant", "content":"Next turn succeeds."}), "stop"),
        ],
    )]).await;
    let request = |message: &str| {
        axum::http::Request::builder()
            .method("POST")
            .uri("/chat/stream")
            .header("authorization", &b.auth_header)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({
                    "message":message, "session_id":b.ctx.session_id,
                    "model_selection":seeded_model_selection(&b.ctx),
                })
                .to_string(),
            ))
            .unwrap()
    };
    let (status, body) =
        collect_full_sse_stream(&b.ctx.app, request("List agent status, then answer."), 60).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let events = parse_sse_events(&body);
    let streamed: String = events
        .iter()
        .filter(|e| e["type"] == "text_delta")
        .map(|e| e["content"].as_str().unwrap())
        .collect();
    assert_eq!(streamed.len(), large_answer.len());
    assert_eq!(Sha256::digest(streamed.as_bytes()), expected_hash);
    let finished = events
        .iter()
        .find(|e| e["type"] == "run_finished")
        .expect("durable terminal before EOF");
    assert_eq!(finished["status"], "failed");
    assert!(!events.iter().any(|e| e["type"] == "turn_complete"));
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "run_error" && e["error_code"] == "resource_limit")
    );
    let run_id = finished["run_id"].as_str().unwrap();
    let row = sqlx::query(
        "SELECT status, terminal_receipt_json, last_event_idx FROM agent_runs WHERE user_id = ? AND run_id = ?",
    )
    .bind(&b.ctx.user_id)
    .bind(run_id)
    .fetch_one(&b.ctx.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("status"), "failed");
    let receipt = CommittedRunTerminalReceipt::decode(
        &row.get::<String, _>("terminal_receipt_json"),
        row.get("last_event_idx"),
    )
    .unwrap()
    .receipt;
    assert_eq!(receipt.outcome, CanonicalTerminalOutcome::Failed);
    assert_eq!(receipt.context, CanonicalTerminalContext::Unchanged);
    assert_eq!(receipt.error_code.as_deref(), Some("resource_limit"));
    let usage = receipt
        .usage
        .expect("provider accounting survives optional Context failure");
    assert_eq!(usage.input_tokens(), Some(126));
    assert_eq!(usage.output_tokens(), Some(21));
    let ledger = receipt
        .tool_ledger
        .expect("real tool ledger survives optional Context failure");
    assert_eq!(ledger.attempted, 2);
    assert_eq!(ledger.terminal, 2);
    assert_eq!(ledger.unresolved, 0);
    assert_eq!(ledger.result_classes.succeeded, 2);
    let stored_answer: String = sqlx::query_scalar(
        "SELECT content FROM session_transcript_items WHERE user_id = ? AND session_id = ? AND run_id = ? AND role = 'assistant'",
    ).bind(&b.ctx.user_id).bind(&b.ctx.session_id).bind(run_id).fetch_one(&b.ctx.pool).await.unwrap();
    assert_eq!(stored_answer.len(), large_answer.len());
    assert_eq!(Sha256::digest(stored_answer.as_bytes()), expected_hash);
    let context_nodes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_manifest_nodes WHERE owner_user_id = ? AND session_id = ?")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&b.ctx.pool).await.unwrap();
    assert_eq!(context_nodes, 0);
    assert_eq!(
        b.ctx.native_provider_requests().await.len(),
        3,
        "Context failure must not retry provider or tools"
    );
    let (_, next) = collect_full_sse_stream(&b.ctx.app, request("hi"), 30).await;
    let next_events = parse_sse_events(&next);
    assert!(
        next_events
            .iter()
            .any(|e| e["type"] == "run_finished" && e["status"] == "completed"),
        "{next}"
    );
    assert!(next.contains("Next turn succeeds."));
    assert_eq!(b.ctx.native_provider_requests().await.len(), 4);
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    b.ctx.close().await;
}
