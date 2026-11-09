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

/// Require the exact durable admission/completion contract. Merely queuing
/// work cannot end capture, and an error/skip cannot be counted as success.
fn chat_observer_progress(
    registry: &astra_turn_core::pipeline_metrics::MetricsRegistry,
    expected_observers: u64,
) -> (u64, u64) {
    let metrics = registry.render_prometheus();
    let mut completed = 0;
    let mut queued = 0;
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
                series, "astra_turn_observer_runs_total{mode=\"canonical\",outcome=\"success\"}",
                "authorized SQL budget requires successful observation, not skipped or failed work: {line}"
            );
            completed += value;
        } else {
            assert_eq!(
                series,
                "astra_turn_observer_dispatches_total{mode=\"canonical\",outcome=\"eligible\"}",
                "authorized SQL budget requires durable observation admission: {line}"
            );
            queued += value;
        }
    }
    assert!(
        queued <= expected_observers,
        "unexpected extra queued observer"
    );
    assert!(completed <= expected_observers, "unexpected extra observer");
    (queued, completed)
}

async fn wait_for_chat_observers(
    registry: Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>,
    expected_observers: u64,
) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if chat_observer_progress(&registry, expected_observers)
                == (expected_observers, expected_observers)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("each chat must complete its durable observer before ending SQL capture");
}

/// build_app fixtures have no server timer owner. Run the same production
/// recovery operation for this exact Session, never a parallel mock worker or
/// a scan over other fixtures. Its acknowledgement must finish before capture
/// ends: the worker's success metric precedes the head checkpoint SQL.
async fn drain_chat_observers(
    ctx: &MatrixE2eCtx,
    owner: &astra_runtime::server::provider_test_support::ObservationRecoveryForTest,
    expected_observers: u64,
) {
    let registry = ctx.app_state.metrics_registry();
    // Cover the production claim/attempt/receipt bounds plus the recovery tick.
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            let (eligible, completed) = chat_observer_progress(&registry, expected_observers);
            if eligible == expected_observers && completed == expected_observers {
                return;
            }
            if eligible > completed {
                // Model the managed recovery cadence, rather than demanding
                // synchronous eligibility at the terminal boundary. MatrixOne
                // statements can briefly see a just-written NOW(6) due marker
                // as future. A no-claim result is normal pending work, not an
                // acknowledged observation; all retry SQL stays in capture.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                owner
                    .drive_one()
                    .await
                    .expect("canonical observation and checkpoint must succeed");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("each chat must checkpoint its canonical observation before ending SQL capture");
}

async fn assert_observer_acknowledged(ctx: &MatrixE2eCtx, expected_turn: i64) {
    let sqlx::types::Json(work) = sqlx::query_scalar::<
        _,
        sqlx::types::Json<astra_services::session_observation::ObservationCheckpoint>,
    >(
        "SELECT observation_json FROM session_context_heads WHERE isolation_domain='server' AND branch_id='main' AND owner_user_id = ? AND session_id = ?",
    )
    .bind(&ctx.user_id)
    .bind(&ctx.session_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(
        i64::from(work.observed_turn),
        expected_turn,
        "the observed canonical turn must have a persisted receipt"
    );
}

// A newly committed database NOW(6) marker need not be eligible to the very
// next statement. Match managed recovery's two-second cadence; only no-work
// results are retried. Authority, materialization and receipt errors still fail.
async fn drive_scoped_observation_when_due(
    owner: &astra_runtime::server::provider_test_support::ObservationRecoveryForTest,
) -> Result<(), String> {
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            if owner.drive_one().await? {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    })
    .await
    .map_err(|_| "scoped observation did not become due and finish within its budget".to_owned())?
}

fn observer_metrics(
    queued: u64,
    successes: u64,
) -> Arc<astra_turn_core::pipeline_metrics::MetricsRegistry> {
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
    registry.register_counter("astra_turn_observer_dispatches_total", "observer test");
    registry.register_counter("astra_turn_observer_runs_total", "observer test");
    registry.increment_counter(
        "astra_turn_observer_dispatches_total",
        &[("mode", "canonical"), ("outcome", "eligible")],
        queued,
    );
    registry.increment_counter(
        "astra_turn_observer_runs_total",
        &[("mode", "canonical"), ("outcome", "success")],
        successes,
    );
    registry
}

#[tokio::test]
async fn sql_budget_requires_the_expected_successful_observations() {
    wait_for_chat_observers(observer_metrics(2, 2), 2).await;
}

#[tokio::test]
async fn sql_budget_rejects_extra_queued_but_pending_observer() {
    let result = tokio::spawn(wait_for_chat_observers(observer_metrics(2, 1), 1)).await;
    assert!(
        result.is_err_and(|error| error.is_panic()),
        "the first success must not end capture while a duplicate queued observer can still issue SQL"
    );
}

#[tokio::test(start_paused = true)]
async fn sql_budget_rejects_queued_observations_without_completion() {
    let result = tokio::spawn(wait_for_chat_observers(observer_metrics(1, 0), 1)).await;
    assert!(
        result.is_err_and(|error| error.is_panic()),
        "durable admission alone must not complete the SQL probe"
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
            registry.increment_counter(metric, &[("mode", "canonical"), ("outcome", outcome)], 1);
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
struct Statement(String, f64, u64);

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

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "rows_returned" {
            self.2 = value;
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

struct ObservedSql {
    sql: String,
    elapsed: std::time::Duration,
    finished_at: std::time::Instant,
    returned_rows: u64,
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
                returned_rows: statement.2,
            });
        }
    }
}

const CONTROL_OBSERVATION_SQL: &str = "SELECT requested.ordinal, run_id, session_id, status, waiting_for, parent_run_id, ancestor_path, run_generation, last_event_idx, CAST(cancellation_requested_at IS NOT NULL AS SIGNED) AS cancellation_requested, CAST(owner_pod_id IS NOT NULL AND owner_lease_expires_at IS NOT NULL AND owner_lease_expires_at >= NOW(6) AS SIGNED) AS owner_lease_live FROM (VALUES ROW(0,?,?)) requested(ordinal, requested_user_id, requested_run_id) LEFT JOIN agent_runs ON user_id = requested_user_id AND run_id = requested_run_id AND run_id IN (?)";

// The real shared control reader batches concurrently active Runs. Match its
// exact statement for 1..=8 keys, including ordinal and candidate-IN cardinality,
// rather than classifying any SELECT against agent_runs as periodic work.
fn is_eight_session_control_observation(sql: &str) -> bool {
    if !sql.starts_with("SELECT requested.ordinal, run_id,") {
        return false;
    }
    (1..=8).any(|slots| {
        let rows = (0..slots)
            .map(|i| format!("ROW({i},?,?)"))
            .collect::<Vec<_>>()
            .join(",");
        let candidates = vec!["?"; slots].join(",");
        sql == CONTROL_OBSERVATION_SQL
            .replace("FROM (VALUES ROW(0,?,?))", &format!("FROM (VALUES {rows})"))
            .replace("run_id IN (?)", &format!("run_id IN ({candidates})"))
    })
}

#[test]
fn batched_control_accounting_requires_the_exact_eight_session_contract() {
    assert!(is_eight_session_control_observation(
        CONTROL_OBSERVATION_SQL
    ));
    let batch = CONTROL_OBSERVATION_SQL
        .replace("ROW(0,?,?)", "ROW(0,?,?),ROW(1,?,?)")
        .replace("run_id IN (?)", "run_id IN (?,?)");
    assert!(is_eight_session_control_observation(&batch));
    assert!(!is_eight_session_control_observation(
        &batch.replace("IN (?,?)", "IN (?)")
    ));
    assert!(!is_eight_session_control_observation(&format!(
        "{batch} UNION ALL SELECT 1"
    )));
}

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

// Bound request/attempt work separately from the real two-second control timer.
// Slow CI hosts may legitimately cross several ticks. Keep every statement in
// the raw total, and bound both the timer rate and the nonperiodic work: elapsed
// time must never excuse an extra business query or repeated auth snapshot.
fn assert_scoped_sql_budget(statements: &[String], core_limit: usize, elapsed_ms: f64) {
    let auth = authentication_snapshot_count(statements);
    let polls = statements
        .iter()
        .filter(|sql| sql.as_str() == CONTROL_OBSERVATION_SQL)
        .count();
    let poll_limit = (elapsed_ms / 2_000.0).ceil() as usize;
    assert!(
        polls <= poll_limit,
        "control polling exceeded its two-second cadence: {polls}/{poll_limit}"
    );
    assert!(
        statements.len() - polls - auth <= core_limit,
        "nonperiodic SQL budget: raw={}, controls={polls}, auth={auth}, limit={core_limit}\n{}",
        statements.len(),
        statements.join("\n")
    );
    assert!(
        statements.len() <= core_limit + auth + poll_limit,
        "raw SQL must fit the core, one auth snapshot and elapsed control-timer envelope"
    );
    eprintln!(
        "scoped_sql_budget raw={} core={} auth={auth} controls={polls} elapsed_ms={elapsed_ms:.0}",
        statements.len(),
        statements.len() - polls - auth
    );
}

#[test]
fn sql_budget_accepts_elapsed_ticks_without_allowing_extra_business_queries() {
    let mut sql = vec!["synthetic business query".to_owned(); 23];
    sql.extend(std::iter::repeat_n(CONTROL_OBSERVATION_SQL.to_owned(), 2));
    assert_scoped_sql_budget(&sql, 23, 4_100.0);
    assert!(std::panic::catch_unwind(|| assert_scoped_sql_budget(&sql, 23, 900.0)).is_err());
    let mut authenticated = sql.clone();
    authenticated.push(AUTHENTICATION_SNAPSHOT_SQL.into());
    assert_scoped_sql_budget(&authenticated, 23, 4_100.0);
    authenticated.push(AUTHENTICATION_SNAPSHOT_SQL.into());
    assert!(
        std::panic::catch_unwind(|| assert_scoped_sql_budget(&authenticated, 23, 30_000.0))
            .is_err()
    );
    sql.push("unexpected business query".into());
    assert!(std::panic::catch_unwind(|| assert_scoped_sql_budget(&sql, 23, 30_000.0)).is_err());
}

fn answer(message: Value, reason: &str) -> ProviderResponse {
    ProviderResponse::OpenAi(json!({
        "choices": [{"index": 0, "message": message, "finish_reason": reason}],
        "usage": {"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49}
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
        "usage":{"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}
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
        let observer =
            astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
                &b.ctx.app_state,
                &b.ctx.user_id,
                &b.ctx.session_id,
            )
            .await
            .unwrap();
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
            drain_chat_observers(&b.ctx, &observer, 1).await;
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
        drain_chat_observers(&b.ctx, &observer, 1 + u64::from(warm_context)).await;
        let samples = capture.0.lock().unwrap().take().unwrap();
        let execution_ms = execution_started.elapsed().as_secs_f64() * 1000.0;
        assert_observer_acknowledged(&b.ctx, 1 + i64::from(warm_context)).await;
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
            use astra_services::session_restore::SessionRestoreService;
            let restore = SessionRestoreService::new(b.ctx.pool.clone());
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
            execution_ms,
        ));
    }
    for (scenario, rounds, warm_context, polls, statements, elapsed_ms) in &budgets {
        // A warm request restores its prior canonical Context once before
        // execution, in addition to the observer's captured-head materialization.
        // This is required history work, independent of readonly tool rounds.
        let context_reads = statements
            .iter()
            .filter(|sql| {
                sql.starts_with("SELECT ") && sql.contains("FROM conversation_manifest_nodes")
            })
            .count();
        assert_eq!(context_reads, 1 + usize::from(*warm_context));
        assert_scoped_sql_budget(statements, 23 + usize::from(*warm_context), *elapsed_ms);
        let authentication_reads = authentication_snapshot_count(statements);
        if *scenario == "ordinary" {
            continue;
        }
        let baseline = budgets
            .iter()
            .find(|(name, _, warm, _, _, _)| *name == "ordinary" && warm == warm_context)
            .unwrap();
        // Explain exact periodic reads and at most one request-auth cache
        // miss. Every query remains in the separately bounded raw total.
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
    // The answer fits the provider stream but exceeds the canonical segment.
    // The live and durable transcript must retain it while model context
    // keeps the preceding tool facts and a bounded placeholder.
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
            .any(|e| e["type"] == "run_error" && e["error_code"] == "execution_incomplete")
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
    assert!(matches!(
        receipt.context,
        CanonicalTerminalContext::Advanced { .. }
    ));
    assert_eq!(receipt.error_code.as_deref(), Some("execution_incomplete"));
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
    assert!(
        context_nodes > 0,
        "the bounded tool history must be retained"
    );
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
    let requests = b.ctx.native_provider_requests().await;
    assert_eq!(requests.len(), 4);
    let next_prompt = requests[3].body.to_string();
    assert!(next_prompt.contains("preceding assistant response exceeded the context limit"));
    assert!(!next_prompt.contains(&"x".repeat(1024)));
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
async fn skill_turns_preserve_results_without_a_selection_projection() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .expect("this integration process owns its SQL subscriber");
    for present in [true, false] {
        let b = bootstrap().await;
        let skill_name = format!("cleanup-probe-{}", b.ctx.suffix);
        {
            sqlx::query("INSERT INTO skills_registry (skill_id, skill_name, version, skill_definition, created_by) VALUES (?, ?, '1.0.0', CAST(? AS JSON), ?)")
                .bind(uuid::Uuid::new_v4().to_string()).bind(&skill_name)
                .bind(json!({"skill_type":"local", "instructions":"Cleanup probe instructions."}).to_string())
                .bind(&b.ctx.user_id).execute(&b.ctx.pool).await.unwrap();
        }
        let requested_skill = if present {
            skill_name.clone()
        } else {
            format!("missing-{}", b.ctx.suffix)
        };
        let model = format!("mock-{}", b.ctx.suffix);
        b.ctx.install_native_provider(&b.auth_header, vec![ProviderScript::new(
            "selected skill and missing skill retain their real results",
            move |request| request.body["model"] == model && request.body["stream"] == true,
            vec![
                answer(json!({"role":"assistant", "content":"", "tool_calls":[{
                    "id":"selected-skill", "type":"function", "function":{"name":"skill",
                    "arguments":json!({"skill_name":requested_skill}).to_string()}
                }]}), "tool_calls"),
                answer(json!({"role":"assistant", "content":"Skill round complete."}), "stop"),
            ],
        )]).await;
        let observer =
            astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
                &b.ctx.app_state,
                &b.ctx.user_id,
                &b.ctx.session_id,
            )
            .await
            .unwrap();
        prime_chat_pool(&b.ctx).await;
        *capture.0.lock().unwrap() = Some(Vec::new());
        let request = axum::http::Request::builder().method("POST").uri("/chat/stream")
            .header("authorization", &b.auth_header).header("content-type", "application/json")
            .body(axum::body::Body::from(json!({"message":"Use the requested skill.",
                "session_id": b.ctx.session_id, "model_selection":seeded_model_selection(&b.ctx),
                "allow_skills":[skill_name],
                "execution_policy":{"turn_intent":"fixed_default", "skill_auto_route":"disabled"}
            }).to_string())).unwrap();
        let (status, body) = collect_full_sse_stream(&b.ctx.app, request, 30).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let events = parse_sse_events(&body);
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
            "{body}"
        );
        assert!(
            b.ctx
                .app_state
                .drain_background_runs(std::time::Duration::from_secs(5))
                .await
        );
        drain_chat_observers(&b.ctx, &observer, 1).await;
        let samples = capture.0.lock().unwrap().take().unwrap();
        assert_observer_acknowledged(&b.ctx, 1).await;
        assert!(!samples.is_empty());
        assert!(
            samples
                .iter()
                .all(|sample| !sample.sql.contains("skill_selection_events")),
            "neither success nor failure may issue a detached selection write"
        );
        let requests = b.ctx.native_provider_requests().await;
        let result = requests.last().unwrap().body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| {
                message["role"] == "tool" && message["tool_call_id"] == "selected-skill"
            })
            .and_then(|message| message["content"].as_str())
            .unwrap_or_else(|| panic!("skill result missing: {body}"));
        if present {
            assert!(result.contains("Cleanup probe instructions."), "{result}");
            assert!(result.contains("<skill-loaded"), "{result}");
        } else {
            assert!(
                result.contains(&format!("Unknown skill `{requested_skill}`")),
                "{result}"
            );
            assert!(!result.contains("<skill-loaded"), "{result}");
        }
        eprintln!(
            "skill_selection present={present} total_sql={} retired_projection_sql=0",
            samples.len()
        );
        b.ctx.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; this process owns SQL capture"]
async fn concurrent_run_projections_preserve_one_hundred_session_identities() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .expect("this integration binary owns its SQL subscriber");
    let b = bootstrap().await;
    let prefix = format!("poll-{}-", uuid::Uuid::new_v4());
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..100_u32 {
        let session_id = format!("{prefix}s{index}");
        let run_id = format!("{prefix}r{index}");
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, status) VALUES (?, ?, 'active')",
        )
        .bind(&session_id)
        .bind(&b.ctx.user_id)
        .execute(&b.ctx.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_runs
             (run_id, user_id, session_id, root_run_id, ancestor_path, depth, retry_scope,
              status, execution_mode, owner_pod_id, owner_lease_expires_at,
              run_generation, last_event_idx, total_prompt_tokens, total_completion_tokens,
              total_tool_calls, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 0, 'node', 'running', 'web_agent', 'poll-fixture',
                     TIMESTAMPADD(MINUTE, 5, NOW(6)), 0, -1, ?, 0, 0, NOW(6), NOW(6))",
        )
        .bind(&run_id)
        .bind(&b.ctx.user_id)
        .bind(&session_id)
        .bind(&run_id)
        .bind(&run_id)
        .bind(index + 1)
        .execute(&b.ctx.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO agent_run_openings (user_id,run_id,session_id) VALUES (?,?,?)")
            .bind(&b.ctx.user_id)
            .bind(&run_id)
            .bind(&session_id)
            .execute(&b.ctx.pool)
            .await
            .unwrap();
    }
    *capture.0.lock().unwrap() = Some(Vec::new());
    for index in 0..100_u32 {
        let app = b.ctx.app.clone();
        let auth = b.auth_header.clone();
        let session_id = format!("{prefix}s{index}");
        let run_id = format!("{prefix}r{index}");
        tasks.spawn(async move {
            let (status, body) = get_json(
                &app,
                &format!("/chat/runs/{run_id}/projection?recent_limit=1"),
                Some(&auth),
                &[],
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::OK, "{body}");
            assert_eq!(body["run_id"], run_id);
            assert_eq!(body["session_id"], session_id);
            assert_eq!(body["status"], "running");
            assert_eq!(body["total_prompt_tokens"], index + 1);
            assert!(body["recent_events"].as_array().unwrap().is_empty());
            assert!(body.get("observability").is_none());
        });
    }
    let mut completed = 0;
    while let Some(result) = tasks.join_next().await {
        result.expect("independent public projection read");
        completed += 1;
    }
    assert_eq!(completed, 100);
    let samples = capture.0.lock().unwrap().take().unwrap();
    assert!(
        !samples.is_empty(),
        "the query observer must cover real database reads"
    );
    assert!(
        samples
            .iter()
            .all(|sample| !sample.sql.contains("prompt_request_records")
                && !sample.sql.contains("prompt_deltas"))
    );
    let (rows, tokens): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), CAST(SUM(total_prompt_tokens) AS SIGNED) FROM agent_runs WHERE user_id = ? AND run_id LIKE ?",
    ).bind(&b.ctx.user_id).bind(format!("{prefix}%")).fetch_one(&b.ctx.pool).await.unwrap();
    assert_eq!((rows, tokens), (100, 5050));
    for sql in [
        "DELETE FROM agent_run_openings WHERE user_id = ? AND run_id LIKE ?",
        "DELETE FROM agent_runs WHERE user_id = ? AND run_id LIKE ?",
        "DELETE FROM agent_sessions WHERE user_id = ? AND session_id LIKE ?",
    ] {
        sqlx::query(sql)
            .bind(&b.ctx.user_id)
            .bind(format!("{prefix}%"))
            .execute(&b.ctx.pool)
            .await
            .unwrap();
    }
    b.ctx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; scripted localhost provider"]
async fn configured_auto_finishes_without_authoring_or_rollout_queries() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .unwrap();
    let b = bootstrap().await;
    struct ClearOverlay;
    impl Drop for ClearOverlay {
        fn drop(&mut self) {
            astra_config::runtime_config::set_cli_overlay(None);
        }
    }
    let _clear = ClearOverlay;
    astra_config::runtime_config::set_cli_overlay(Some(
        astra_config::config_overlay::RuntimeConfigLayer::from_json(&json!({
            "model_routing": {"revision": "sql-budget-pair", "strong_offering_id": b.ctx.model_offering_id,
                "economy_offering_id": "unavailable-economy"}
        }).to_string()).unwrap()
    ));
    // No judgment Offering is configured in this fixture. Auto must freeze
    // its strong fallback through the real shared pool before primary dispatch.
    let model = format!("mock-{}", b.ctx.suffix);
    b.ctx
        .install_native_provider(
            &b.auth_header,
            vec![ProviderScript::new(
                "Auto primary",
                move |r| r.body["model"] == model && r.body["stream"] == true,
                vec![answer(
                    json!({"role":"assistant", "content":"Auto completed."}),
                    "stop",
                )],
            )],
        )
        .await;
    let observer = astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
        &b.ctx.app_state,
        &b.ctx.user_id,
        &b.ctx.session_id,
    )
    .await
    .unwrap();
    prime_chat_pool(&b.ctx).await;
    let execution_started = std::time::Instant::now();
    *capture.0.lock().unwrap() = Some(Vec::new());
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", &b.auth_header)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({"message":"Reply briefly.", "session_id":b.ctx.session_id,
            "execution_policy":{"model_routing":"auto"}})
            .to_string(),
        ))
        .unwrap();
    let (status, body) = collect_full_sse_stream(&b.ctx.app, request, 30).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert!(
        parse_sse_events(&body)
            .iter()
            .any(|e| e["type"] == "run_finished" && e["status"] == "completed"),
        "{body}"
    );
    assert!(body.contains("Auto completed."), "{body}");
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    drain_chat_observers(&b.ctx, &observer, 1).await;
    assert_eq!(
        b.ctx.native_provider_requests().await.len(),
        1,
        "unconfigured optional judgment must not send an auxiliary provider request"
    );
    let statements = capture.0.lock().unwrap().take().unwrap();
    let execution_ms = execution_started.elapsed().as_secs_f64() * 1000.0;
    assert_observer_acknowledged(&b.ctx, 1).await;
    assert!(!statements.is_empty());
    let sql = statements
        .iter()
        .map(|sample| sample.sql.clone())
        .collect::<Vec<_>>();
    assert_eq!(sql.iter().filter(|statement| statement.starts_with("SELECT payload_json, event_idx FROM agent_run_events WHERE user_id = ? AND run_id = ? AND event_type = ? AND idempotency_key = ?")).count(), 1,
        "intent judgment and Auto preparation must reuse the same owned-Run decision lookup");
    assert_scoped_sql_budget(&sql, 33, execution_ms);
    let control_polls = sql
        .iter()
        .filter(|statement| statement.as_str() == CONTROL_OBSERVATION_SQL)
        .count();
    let authentication_reads = authentication_snapshot_count(&sql);
    for sample in &statements {
        for retired in [
            "model_router_deployments",
            "agent_agents",
            "session_state_items",
            "user_skill_sources",
            "user_skill_versions",
        ] {
            assert!(
                !sample.sql.contains(retired),
                "retired query: {}",
                sample.sql
            );
        }
    }
    let saved: String = sqlx::query_scalar("SELECT payload_json FROM agent_run_events WHERE user_id = ? AND session_id = ? AND event_type = 'model_routing_decision'")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&b.ctx.pool).await.unwrap();
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["data"]["policy_version"], "easy-read-only-v1");
    assert_eq!(
        saved["data"]["selected_offering_id"],
        b.ctx.model_offering_id
    );
    assert!(saved["data"].get("rollout").is_none());
    eprintln!(
        "configured_auto total_sql={} control_polls={} authentication_reads={} reason={} retired_query_count=0 complete_drain=true",
        statements.len(),
        control_polls,
        authentication_reads,
        saved["data"]["reason"]
    );
    b.ctx.close().await;
}

#[derive(Default)]
struct FailingOnceObserver {
    calls: Mutex<Vec<astra_runtime::TurnObserverRequest>>,
}
#[async_trait::async_trait]
impl astra_runtime::TurnObserverWorker for FailingOnceObserver {
    async fn run(
        &self,
        request: astra_runtime::TurnObserverRequest,
    ) -> Result<astra_runtime::TurnObserverOutcome, String> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(request);
        if calls.len() == 1 {
            Err("synthetic transient failure".into())
        } else {
            Ok(astra_runtime::TurnObserverOutcome::Observed)
        }
    }
}

async fn scripted_observation_turns(ctx: &MatrixE2eCtx, auth: &str, count: usize) {
    let model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(
        auth,
        vec![ProviderScript::new(
            "canonical observation turns",
            move |request| request.body["model"] == model && request.body["stream"] == true,
            (0..count)
                .map(|_| {
                    answer(
                        json!({"role":"assistant","content":"Canonical answer."}),
                        "stop",
                    )
                })
                .collect(),
        )],
    )
    .await;
}
async fn complete_observation_turn(ctx: &MatrixE2eCtx, auth: &str) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({"message":"Remember this synthetic fact.",
            "session_id":ctx.session_id, "model_selection":seeded_model_selection(ctx)})
            .to_string(),
        ))
        .unwrap();
    let (status, body) = collect_full_sse_stream(&ctx.app, request, 30).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert!(
        parse_sse_events(&body)
            .iter()
            .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; scripted localhost HTTP"]
async fn canonical_observer_backoff_survives_new_terminal_turn() {
    require_system_e2e_env();
    let b = bootstrap().await;
    scripted_observation_turns(&b.ctx, &b.auth_header, 2).await;
    let worker = Arc::new(FailingOnceObserver::default());
    let state = b
        .ctx
        .app_state
        .clone()
        .with_turn_observer_worker(worker.clone());
    let owner = astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
        &state,
        &b.ctx.user_id,
        &b.ctx.session_id,
    )
    .await
    .unwrap();
    complete_observation_turn(&b.ctx, &b.auth_header).await;
    let store = astra_services::session_observation::SessionObservationStore::new(
        b.ctx.shared_pool.clone(),
    );
    let stale = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(candidate) = store
                .claim(&b.ctx.user_id, &b.ctx.session_id)
                .await
                .unwrap()
            {
                break candidate;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    })
    .await
    .expect("canonical completion must become claimable");
    store.expire_leases_for_test().await;
    drive_scoped_observation_when_due(&owner).await.unwrap();
    assert!(store.finish(&stale, true).await.is_err());
    let work: sqlx::types::Json<astra_services::session_observation::ObservationCheckpoint> =
        sqlx::query_scalar("SELECT observation_json FROM session_context_heads WHERE isolation_domain='server' AND owner_user_id=? AND session_id=? AND branch_id='main'")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&b.ctx.pool).await.unwrap();
    assert_eq!(work.0.observed_turn, 0);
    assert_eq!(work.0.attempts, 1);
    sqlx::query("UPDATE session_context_heads SET observation_due_at = DATE_ADD(NOW(6), INTERVAL 60 SECOND) WHERE isolation_domain='server' AND owner_user_id=? AND session_id=? AND branch_id='main'")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).execute(&b.ctx.pool).await.unwrap();
    let before: String = sqlx::query_scalar("SELECT observation_json FROM session_context_heads WHERE isolation_domain='server' AND owner_user_id=? AND session_id=? AND branch_id='main'")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&b.ctx.pool).await.unwrap();
    complete_observation_turn(&b.ctx, &b.auth_header).await;
    astra_services::session_memory_lifecycle::SessionMemoryLifecycleStore::new(
        b.ctx.shared_pool.clone(),
    )
    .request_extraction(&b.ctx.user_id, &b.ctx.session_id)
    .await
    .unwrap();
    let after: String = sqlx::query_scalar("SELECT observation_json FROM session_context_heads WHERE isolation_domain='server' AND owner_user_id=? AND session_id=? AND branch_id='main'")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&b.ctx.pool).await.unwrap();
    assert_eq!(
        before, after,
        "a new terminal turn must not reset a failed observation's retry clock"
    );
    assert!(
        !owner.drive_one().await.unwrap(),
        "backed-off Session must remain ineligible"
    );
    assert_eq!(worker.calls.lock().unwrap().len(), 1);
    // Simulate restart by constructing a new recovery owner, and expire only
    // this test's Session lease. Recovery derives the latest input from Context.
    owner.expire_lease().await;
    assert!(store.finish(&stale, true).await.is_err());
    let expired: String = sqlx::query_scalar("SELECT observation_json FROM session_context_heads WHERE isolation_domain='server' AND owner_user_id=? AND session_id=? AND branch_id='main'")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&b.ctx.pool).await.unwrap();
    assert_eq!(after, expired, "expired lease owners cannot checkpoint");
    sqlx::query("UPDATE session_context_heads SET observation_due_at=NOW(6) WHERE isolation_domain='server' AND owner_user_id=? AND session_id=? AND branch_id='main'")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).execute(&b.ctx.pool).await.unwrap();
    let restarted = astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
        &state,
        &b.ctx.user_id,
        &b.ctx.session_id,
    )
    .await
    .unwrap();
    drive_scoped_observation_when_due(&restarted).await.unwrap();
    assert_observer_acknowledged(&b.ctx, 2).await;
    assert_eq!(
        worker
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.turn_count)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    let retained: bool = sqlx::query_scalar("SELECT JSON_UNQUOTE(JSON_EXTRACT(state_json, '$.extraction_requested')) = 'true' FROM agent_session_memory_work WHERE user_id = ? AND session_id = ?")
        .bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&b.ctx.pool).await.unwrap();
    assert!(
        retained,
        "observation checkpoint must preserve lifecycle work"
    );
    assert!(
        !restarted.drive_one().await.unwrap(),
        "checkpointed Context must not be observed repeatedly"
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
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; scripted localhost HTTP"]
async fn unconfigured_server_chat_has_no_observation_sql() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .unwrap();
    let mut b = bootstrap().await;
    let state = b.ctx.app_state.clone().with_turn_observer_worker(Arc::new(
        astra_runtime::DatabaseTurnObserverWorker::new(None),
    ));
    b.ctx.app = astra_runtime::build_app(state.clone());
    b.ctx.app_state = state;
    scripted_observation_turns(&b.ctx, &b.auth_header, 1).await;
    assert!(
        astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
            &b.ctx.app_state,
            &b.ctx.user_id,
            &b.ctx.session_id
        )
        .await
        .is_none()
    );
    prime_chat_pool(&b.ctx).await;
    let execution_started = std::time::Instant::now();
    *capture.0.lock().unwrap() = Some(Vec::new());
    complete_observation_turn(&b.ctx, &b.auth_header).await;
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    let samples = capture.0.lock().unwrap().take().unwrap();
    assert!(!samples.is_empty());
    assert_scoped_sql_budget(
        &samples
            .iter()
            .map(|sample| sample.sql.clone())
            .collect::<Vec<_>>(),
        20,
        execution_started.elapsed().as_secs_f64() * 1000.0,
    );
    assert!(
        samples
            .iter()
            .all(|sample| !sample.sql.contains("agent_session_memory_work")
                && !sample.sql.contains("sweeper_leases")),
        "unconfigured observer must issue no journal or lease SQL"
    );
    b.ctx.close().await;
}

struct HeldCanonicalObserver {
    entered: tokio::sync::Notify,
    permits: tokio::sync::Semaphore,
    seen: Mutex<std::collections::BTreeSet<String>>,
    fail_once: std::sync::atomic::AtomicBool,
    failed_session: Mutex<Option<String>>,
}
#[async_trait::async_trait]
impl astra_runtime::TurnObserverWorker for HeldCanonicalObserver {
    async fn run(
        &self,
        request: astra_runtime::TurnObserverRequest,
    ) -> Result<astra_runtime::TurnObserverOutcome, String> {
        self.entered.notify_one();
        self.permits.acquire().await.unwrap().forget();
        if self
            .fail_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            *self.failed_session.lock().unwrap() = Some(request.session_id);
            return Err("synthetic first-Session failure".into());
        }
        self.seen.lock().unwrap().insert(request.session_id);
        Ok(astra_runtime::TurnObserverOutcome::Observed)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; 128 synthetic canonical Sessions"]
async fn canonical_observation_burst_recovers_without_chat_job_writes() {
    require_system_e2e_env();
    let started = std::time::Instant::now();
    // Leave 20s of the strict online budget for both idle windows and cleanup.
    let recovery_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(40);
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .expect("this integration binary owns its SQL subscriber");
    use sqlx::Connection;
    let mut settings = astra_core::MatrixOneSettings::from_env();
    settings.database = "mysql".into();
    let mut admin = sqlx::MySqlConnection::connect(&settings.database_url_with_password())
        .await
        .unwrap();
    let database = format!(
        "astra_test_http_handoff_obs_{}",
        uuid::Uuid::new_v4().simple()
    );
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
        .execute(&mut admin)
        .await
        .unwrap();
    let b = bootstrap_isolated_execution_handoff(database.clone()).await;
    eprintln!(
        "observation_burst phase=bootstrap elapsed_ms={}",
        started.elapsed().as_millis()
    );
    // Leave full empty physical intervals before the real backlog. Discovery
    // must advance across deletion/allocation gaps, not stop at an empty page.
    sqlx::query("INSERT INTO session_context_heads (isolation_domain,owner_user_id,session_id,branch_id) SELECT 'server','catalog-gap-owner',CAST(result AS CHAR),'main' FROM generate_series(1,4096) g")
        .execute(&b.ctx.pool).await.unwrap();
    sqlx::query("DELETE FROM session_context_heads WHERE owner_user_id='catalog-gap-owner'")
        .execute(&b.ctx.pool)
        .await
        .unwrap();
    // Two full 64-key discovery pages plus paced arrivals exercise backlog
    // recovery within the unchanged 60s CI budget, including setup and idle
    // checks. Large-catalog selectivity has its own 100,000-head regression.
    let mut sessions: Vec<_> = (0..128).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    // These formerly collided at 32 x 64 catalog slots. All must progress
    // independently while the first Session remains held on another owner.
    sessions[0] = "sparse-00000001".into();
    sessions[1] = "sparse-00002048".into();
    sessions[2] = "sparse-00002049".into();
    seed_observation_sessions(&b.ctx, &sessions).await;
    eprintln!(
        "observation_burst phase=seed sessions={} elapsed_ms={}",
        sessions.len(),
        started.elapsed().as_millis()
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_memory_work")
        .fetch_one(&b.ctx.pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 0,
        "canonical completion must not require observation jobs"
    );
    let probe = Arc::new(SchedulerProbe {
        slow: sessions[0].clone(),
        slow_once: std::sync::atomic::AtomicBool::new(true),
        slow_owner: std::sync::atomic::AtomicUsize::new(usize::MAX),
        in_flight: Default::default(),
        entered: Default::default(),
        permits: tokio::sync::Semaphore::new(0),
        seen: Default::default(),
        active: Default::default(),
        peak: Default::default(),
    });
    *capture.0.lock().unwrap() = Some(Vec::new());
    let mut owners = Vec::new();
    let mut cancels = Vec::new();
    let mut handles = Vec::new();
    for index in 0..2 {
        let state =
            b.ctx
                .app_state
                .clone()
                .with_turn_observer_worker(Arc::new(SchedulerObserver {
                    probe: probe.clone(),
                    owner: index,
                }));
        let owner = Arc::new(
            astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new_catalog(
                &state,
            )
            .await
            .unwrap(),
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let running = owner.clone();
        let token = cancel.clone();
        handles.push(Some(tokio::spawn(async move {
            running.run(token).await;
        })));
        cancels.push(cancel);
        owners.push(owner);
    }
    tokio::time::timeout_at(recovery_deadline, probe.entered.notified())
        .await
        .unwrap();
    wait_for_probe(&probe, recovery_deadline, "first_page", |seen| {
        seen.len() >= 64
    })
    .await;
    eprintln!(
        "observation_burst phase=first_page elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert!(
        !probe.seen.lock().unwrap().contains_key(&sessions[0]),
        "slow Session must still be held"
    );
    let arrivals: Vec<_> = (0..16).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let expected_sessions = sessions.len() + arrivals.len();
    for batch in arrivals.chunks(2) {
        seed_observation_sessions(&b.ctx, batch).await;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    // Fairness is independent progress while this exact call is held, not a
    // promise to drain the backlog before its 60s deadline.
    wait_for_probe(&probe, recovery_deadline, "fairness", |seen| {
        seen.contains_key(&sessions[1]) && seen.contains_key(&sessions[2])
    })
    .await;
    assert!(
        !probe.seen.lock().unwrap().contains_key(&sessions[0]),
        "unrelated Sessions must progress before the held call finishes"
    );
    assert_eq!(
        probe
            .seen
            .lock()
            .unwrap()
            .values()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        [0, 1].into_iter().collect(),
        "both managed owners must serve independent Sessions"
    );
    for peak in &probe.peak {
        assert!(peak.load(std::sync::atomic::Ordering::SeqCst) <= 8);
    }
    let held_owner = probe.slow_owner.load(std::sync::atomic::Ordering::SeqCst);
    cancels[held_owner].cancel();
    tokio::time::timeout_at(recovery_deadline, handles[held_owner].take().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        probe.active[held_owner].load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    owners[held_owner].expire_lease().await;
    eprintln!(
        "observation_burst phase=handoff elapsed_ms={}",
        started.elapsed().as_millis()
    );
    wait_for_probe(&probe, recovery_deadline, "handoff", |seen| {
        sessions
            .iter()
            .chain(&arrivals)
            .all(|key| seen.contains_key(key))
            && seen.len() == expected_sessions
    })
    .await;
    // Backend completion precedes durable acknowledgement. Do not cancel the
    // owner in that gap and mistake a deliberately replayable write for loss.
    tokio::time::timeout_at(recovery_deadline, async {
        loop {
            let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_context_heads WHERE JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.observed_turn')) = '1'")
                .fetch_one(&b.ctx.pool).await.unwrap();
            if receipts == expected_sessions as i64 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("every observed Session must receive a durable checkpoint");
    eprintln!(
        "observation_burst phase=receipts elapsed_ms={}",
        started.elapsed().as_millis()
    );
    for (cancel, handle) in cancels.iter().zip(handles.iter_mut()) {
        cancel.cancel();
        if let Some(handle) = handle.take() {
            tokio::time::timeout_at(recovery_deadline, handle)
                .await
                .unwrap()
                .unwrap();
        }
    }
    let active_sql = capture.0.lock().unwrap().take().unwrap();
    assert!(
        active_sql.len() <= 20 * expected_sessions,
        "full two-owner recovery + paced completion SQL: {}",
        active_sql.len()
    );
    assert!(
        active_sql
            .iter()
            .any(|q| q.sql.contains("observation_due_at <= NOW(6)") && q.sql.contains("LIMIT 64"))
    );
    assert!(
        active_sql
            .iter()
            .all(|q| !q.sql.contains("sweeper_leases") && !q.sql.contains("AUTO_INCREMENT"))
    );
    eprintln!(
        "observation_sql fleet=2 phase=active sessions={expected_sessions} raw_total={}",
        active_sql.len()
    );
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_context_heads WHERE JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.observed_turn')) = '1'")
        .fetch_one(&b.ctx.pool).await.unwrap();
    assert_eq!(receipts, expected_sessions as i64);
    // Restart real managed owners with no cached keys. Idle retained history
    // and deletion gaps must require only one indexed read per owner/tick,
    // with no lease writes, checkpoint writes or transcript reads.
    for retained in [true, false] {
        if !retained {
            sqlx::query("DELETE FROM session_context_heads")
                .execute(&b.ctx.pool)
                .await
                .unwrap();
        }
        let mut idle = Vec::new();
        *capture.0.lock().unwrap() = Some(Vec::new());
        for index in 0..2 {
            let state =
                b.ctx
                    .app_state
                    .clone()
                    .with_turn_observer_worker(Arc::new(SchedulerObserver {
                        probe: probe.clone(),
                        owner: index,
                    }));
            let owner = astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new_catalog(&state).await.unwrap();
            let cancel = tokio_util::sync::CancellationToken::new();
            let token = cancel.clone();
            idle.push((cancel, tokio::spawn(async move { owner.run(token).await })));
        }
        tokio::time::sleep(std::time::Duration::from_millis(5200)).await;
        for (cancel, handle) in idle {
            cancel.cancel();
            handle.await.unwrap();
        }
        let statements = capture.0.lock().unwrap().take().unwrap();
        // The shared HTTP fixture renews its test-account cleanup lease every
        // 30s. That timer can overlap either 5.2s interval. Keep it in the raw
        // total and account for this exact fixture-only statement separately;
        // never allow an observation write or arbitrary query in an idle lane.
        let fixture_heartbeat_sql = "UPDATE auth_users SET last_login_at = CURRENT_TIMESTAMP(6) WHERE user_id = ? AND is_active = 1";
        let fixture_heartbeats = statements
            .iter()
            .filter(|q| q.sql == fixture_heartbeat_sql)
            .count();
        assert!(fixture_heartbeats <= 1);
        assert!(
            (2..=8).contains(&(statements.len() - fixture_heartbeats)),
            "idle fleet SQL grew with history: {}",
            statements.len()
        );
        assert!(
            statements.iter().all(|q| q.sql == fixture_heartbeat_sql
                || (q
                    .sql
                    .starts_with("SELECT owner_user_id, session_id FROM session_context_heads")
                    && q.sql.contains("LIMIT 64"))),
            "idle recovery must only poll due identities: {:?}",
            statements.iter().map(|q| &q.sql).collect::<Vec<_>>()
        );
        eprintln!(
            "observation_sql fleet=2 phase=idle retained={retained} seconds=5.2 fixture_heartbeats={fixture_heartbeats} raw_total={}",
            statements.len()
        );
        eprintln!(
            "observation_burst phase=idle retained={retained} elapsed_ms={}",
            started.elapsed().as_millis()
        );
    }
    b.ctx.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
        .execute(&mut admin)
        .await
        .unwrap();
    eprintln!(
        "observation_burst phase=cleanup elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

/// Full public request + real unscoped recovery + production HTTP observer.
/// Repeated turns, rather than many one-turn Sessions, expose quadratic history work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn repeated_chat_observation_has_linear_history_work() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .unwrap();
    // A real unscoped owner must not consume other concurrently running test
    // fixtures in the shared catalog. Give this entire workload its own schema.
    use sqlx::Connection;
    let mut settings = astra_core::MatrixOneSettings::from_env();
    settings.database = "mysql".into();
    let mut admin = sqlx::MySqlConnection::connect(&settings.database_url_with_password())
        .await
        .unwrap();
    let database = format!(
        "astra_test_http_handoff_round_{}",
        uuid::Uuid::new_v4().simple()
    );
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
        .execute(&mut admin)
        .await
        .unwrap();
    let b = bootstrap_isolated_execution_handoff(database.clone()).await;
    let sessions: Vec<_> = (0..8).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    for session in &sessions {
        sqlx::query("INSERT INTO agent_sessions (user_id,session_id,status) VALUES (?,?,'active')")
            .bind(&b.ctx.user_id)
            .bind(session)
            .execute(&b.ctx.pool)
            .await
            .unwrap();
    }
    let model = format!("mock-{}", b.ctx.suffix);
    let responses = (0..128)
        .map(|_| {
            answer(
                json!({"role":"assistant","content":"Synthetic answer. ".repeat(32)}),
                "stop",
            )
        })
        .collect();
    b.ctx
        .install_native_provider(
            &b.auth_header,
            vec![ProviderScript::new(
                "repeated concurrent observation",
                move |request| request.body["model"] == model,
                responses,
            )],
        )
        .await;
    let bodies: Arc<Mutex<Vec<(Value, usize)>>> = Arc::default();
    let recorded = bodies.clone();
    let app = axum::Router::new().route(
        "/v1/observe",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let recorded = recorded.clone();
                async move {
                    if let Err(status) = memoria_observer_auth::validate_observer_auth(
                        &headers,
                        "synthetic-master-key",
                        None,
                    ) {
                        return status;
                    }
                    let value = serde_json::from_slice(&body).unwrap();
                    recorded.lock().unwrap().push((value, body.len()));
                    axum::http::StatusCode::NO_CONTENT
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let state = b.ctx.app_state.clone().with_turn_observer_worker(Arc::new(
        astra_runtime::DatabaseTurnObserverWorker::new(Some(Arc::new(
            astra_runtime::HttpMemoriaPort::self_hosted(url, "synthetic-master-key".into()),
        ))),
    ));
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut owners = Vec::new();
    // Start capture before discovery: no prepared scoped claim or excluded SQL.
    let measurement_start = std::time::Instant::now();
    *capture.0.lock().unwrap() = Some(Vec::new());
    for _ in 0..2 {
        let owner =
            astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new_catalog(
                &state,
            )
            .await
            .unwrap();
        let token = cancel.clone();
        owners.push(tokio::spawn(async move {
            owner.run(token).await;
        }));
    }
    let mut first_read_bytes = 0;
    let mut first_payload_bytes = 0;
    for turn in 1..=16 {
        let history =
            astra_core::history_work::HistoryWorkScenario::begin("repeated-observation").unwrap();
        let start = std::time::Instant::now();
        let results = futures_util::future::join_all(sessions.iter().map(|session| {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/chat/stream")
                .header("authorization", &b.auth_header)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "message":format!("turn={turn:02} {}", "Synthetic fact. ".repeat(32)),
                        "session_id":session,"model_selection":seeded_model_selection(&b.ctx)
                    })
                    .to_string(),
                ))
                .unwrap();
            collect_full_sse_stream(&b.ctx.app, request, 60)
        }))
        .await;
        for (status, body) in results {
            assert_eq!(status, axum::http::StatusCode::OK, "{body}");
            assert!(
                parse_sse_events(&body)
                    .iter()
                    .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
                "{body}"
            );
        }
        let response_ms = start.elapsed().as_millis();
        tokio::time::timeout(std::time::Duration::from_secs(90), async {
            loop {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_context_heads WHERE owner_user_id=? AND JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.observed_turn'))=? AND observation_lease_token IS NULL")
                    .bind(&b.ctx.user_id).bind(turn.to_string()).fetch_one(&b.ctx.pool).await.unwrap();
                if count == 8 { break; }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }).await.expect("all public chat turns must receive durable observation receipts");
        let report = history.finish().unwrap();
        let read = report
            .scoped
            .measurement(astra_core::history_work::HistoryWorkSite::CanonicalObservationRead);
        assert_eq!(read.rows, 8, "each Session must read only its new manifest");
        // Measure the actual received buffer, independent of optional tracing
        // instrumentation and without serializing the payload again to count it.
        let payload_bytes: u64 = {
            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), turn * 8);
            bodies[(turn - 1) * 8..]
                .iter()
                .map(|(_, bytes)| *bytes as u64)
                .sum()
        };
        if turn == 1 {
            first_read_bytes = read.bytes;
            first_payload_bytes = payload_bytes;
        }
        assert!(
            read.bytes <= first_read_bytes + 1024,
            "history JSON grew with prior turns"
        );
        assert_eq!(
            payload_bytes, first_payload_bytes,
            "HTTP payload must not include prior turns"
        );
        {
            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), turn * 8);
            for (body, _) in &bodies[(turn - 1) * 8..] {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 2);
                assert!(
                    messages[0]["content"]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("turn={turn:02} "))
                );
            }
        }
        eprintln!(
            "observation_round sessions=8 turn={turn} response_ms={response_ms} receipt_ms={} read_rows={} read_json_bytes={} payload_bytes={}",
            start.elapsed().as_millis(),
            read.rows,
            read.bytes,
            payload_bytes
        );
    }
    cancel.cancel();
    for owner in owners {
        owner.await.unwrap();
    }
    let raw = capture.0.lock().unwrap().take().unwrap();
    let verification = raw
        .iter()
        .filter(|sample| {
            sample
                .sql
                .starts_with("SELECT COUNT(*) FROM session_context_heads")
        })
        .count();
    let controls = raw
        .iter()
        .filter(|sample| is_eight_session_control_observation(&sample.sql))
        .count();
    assert!(controls <= 8 * measurement_start.elapsed().as_millis().div_ceil(2_000) as usize);
    let heartbeats = raw
        .iter()
        .filter(|sample| {
            sample
                .sql
                .starts_with("UPDATE auth_users SET last_login_at")
        })
        .count();
    let metadata = raw
        .iter()
        .filter(|sample| {
            sample
                .sql
                .starts_with("UPDATE session_context_heads SET observation_lease_token")
                || sample
                    .sql
                    .starts_with("SELECT head_json, completed_turn, observation_json")
                || sample
                    .sql
                    .starts_with("UPDATE session_context_heads SET observation_json")
                || sample.sql.starts_with(
                    "SELECT JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.receipt_id'))",
                )
        })
        .count();
    let discovery = raw
        .iter()
        .filter(|sample| {
            sample
                .sql
                .starts_with("SELECT owner_user_id, session_id FROM session_context_heads")
        })
        .count();
    let authentication = raw
        .iter()
        .filter(|sample| sample.sql == AUTHENTICATION_SNAPSHOT_SQL)
        .count();
    assert!(
        authentication <= 128,
        "at most one authentication snapshot per request in this workload"
    );
    let pool_setup = raw.iter().filter(|sample| sample.sql == "SET sql_mode=(SELECT CONCAT(@@sql_mode, ',PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION')),time_zone='+00:00',NAMES utf8mb4;").count();
    assert!(
        pool_setup
            <= (b.ctx.pool.options().get_max_connections()
                + b.ctx.shared_pool.get().options().get_max_connections()) as usize
    );
    // Retain a strict frontend bound as well as the complete raw ceiling. In
    // this static-authority profile, each warm request uses 17 core statements;
    // each successful observer adds 4 receipt/claim statements + 1 content read.
    // Losing claims and discovery remain in raw, separately counted, not hidden.
    let frontend =
        raw.len() - metadata - discovery - verification - controls - heartbeats - pool_setup - 128;
    eprintln!(
        "observation_sql_breakdown raw={} frontend={frontend} metadata={metadata} discovery={discovery} verification={verification} controls={controls} fixture_heartbeats={heartbeats} authentication={authentication} pool_setup={pool_setup}",
        raw.len()
    );
    if frontend > 17 * 128 + authentication {
        let mut shapes = std::collections::BTreeMap::new();
        for sample in &raw {
            *shapes
                .entry(sample.sql.chars().take(150).collect::<String>())
                .or_insert(0_usize) += 1;
        }
        eprintln!("observation_sql_shape_counts={shapes:?}");
    }
    assert!(
        frontend <= 17 * 128 + authentication,
        "frontend SQL grew beyond its canonical request contract: {frontend}"
    );
    assert!(
        raw.len() <= 32 * 128 + verification + controls + heartbeats,
        "full public chat plus two-owner recovery SQL: {}",
        raw.len()
    );
    eprintln!(
        "observation_repeated sessions=8 turns=16 raw_sql={} frontend_sql={frontend} recovery_metadata_sql={metadata} discovery_sql={discovery} verification_sql={verification} control_sql={controls} fixture_heartbeat_sql={heartbeats} pool_setup_sql={pool_setup} sql_elapsed_ms={:.0} returned_rows={}",
        raw.len(),
        raw.iter()
            .map(|q| q.elapsed.as_secs_f64() * 1000.0)
            .sum::<f64>(),
        raw.iter().map(|q| q.returned_rows).sum::<u64>()
    );
    assert!(raw.iter().map(|q| q.returned_rows).sum::<u64>() >= 128);
    server.abort();
    let _ = server.await;
    b.ctx.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
        .execute(&mut admin)
        .await
        .unwrap();
}

async fn seed_observation_sessions(ctx: &MatrixE2eCtx, sessions: &[String]) {
    use astra_services::{
        AcquireWriterAndReserveTurnOutcome, DatabaseSessionContextCoordinator,
        SessionContextCoordinator,
    };
    use astra_turn_types::{
        ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
        CanonicalDeltaModeV1, CanonicalTurnDeltaV1, SessionKeyV1, SessionSurfaceV1,
    };
    use futures_util::{StreamExt, stream};
    let coordinator = DatabaseSessionContextCoordinator::new(ctx.shared_pool.clone());
    let actor = ActorContextV1::owner_user(
        &ctx.user_id,
        "synthetic-observation",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    stream::iter(sessions).for_each_concurrent(4, |session| {
        let coordinator = &coordinator;
        let actor = &actor;
        async move {
            sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status) VALUES (?, ?, 'active')")
                .bind(&ctx.user_id).bind(session).execute(&ctx.pool).await.unwrap();
            let key = SessionKeyV1::owner_session("server", &ctx.user_id, session, "main");
            let reservation = match coordinator.acquire_writer_and_reserve_turn(&key, None, actor,
                std::time::Duration::from_secs(30), "writer", "turn", None).await.unwrap() {
                AcquireWriterAndReserveTurnOutcome::Ready { reservation, .. } => reservation,
                other => panic!("unexpected canonical admission: {other:?}"),
            };
            coordinator.commit_turn_and_release_writer(&reservation, CanonicalTurnDeltaV1 {
                schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION, completed_turn: 1, journal_event_seq: 1,
                conversation_seq: 1, compaction_generation: 0, config_version_id: None, mode: CanonicalDeltaModeV1::Append,
                logical_segments: vec![vec![json!({"role":"user","content":"Synthetic burst fact."}),
                    json!({"role":"assistant","content":"Acknowledged."})]],
            }, "commit").await.unwrap();
        }
    }).await;
}

struct SchedulerProbe {
    slow: String,
    slow_once: std::sync::atomic::AtomicBool,
    slow_owner: std::sync::atomic::AtomicUsize,
    in_flight: Mutex<std::collections::BTreeSet<String>>,
    entered: tokio::sync::Notify,
    permits: tokio::sync::Semaphore,
    seen: Mutex<std::collections::BTreeMap<String, usize>>,
    active: [std::sync::atomic::AtomicUsize; 2],
    peak: [std::sync::atomic::AtomicUsize; 2],
}
struct SchedulerObserver {
    probe: Arc<SchedulerProbe>,
    owner: usize,
}
struct ActiveProbeCall(Arc<SchedulerProbe>, usize, String);
impl Drop for ActiveProbeCall {
    fn drop(&mut self) {
        self.0.active[self.1].fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        self.0.in_flight.lock().unwrap().remove(&self.2);
    }
}
#[async_trait::async_trait]
impl astra_runtime::TurnObserverWorker for SchedulerObserver {
    async fn run(
        &self,
        request: astra_runtime::TurnObserverRequest,
    ) -> Result<astra_runtime::TurnObserverOutcome, String> {
        use std::sync::atomic::Ordering::SeqCst;
        assert!(
            self.probe
                .in_flight
                .lock()
                .unwrap()
                .insert(request.session_id.clone()),
            "two owners must not dispatch the same live Session claim concurrently"
        );
        let current = self.probe.active[self.owner].fetch_add(1, SeqCst) + 1;
        self.probe.peak[self.owner].fetch_max(current, SeqCst);
        let _active = ActiveProbeCall(self.probe.clone(), self.owner, request.session_id.clone());
        if request.session_id == self.probe.slow && self.probe.slow_once.swap(false, SeqCst) {
            self.probe.slow_owner.store(self.owner, SeqCst);
            self.probe.entered.notify_one();
            self.probe.permits.acquire().await.unwrap().forget();
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        self.probe
            .seen
            .lock()
            .unwrap()
            .insert(request.session_id, self.owner);
        Ok(astra_runtime::TurnObserverOutcome::Observed)
    }
}
async fn wait_for_probe(
    probe: &SchedulerProbe,
    deadline: tokio::time::Instant,
    phase: &str,
    ready: impl Fn(&std::collections::BTreeMap<String, usize>) -> bool,
) {
    tokio::time::timeout_at(deadline, async {
        loop {
            if ready(&probe.seen.lock().unwrap()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let observed = probe.seen.lock().unwrap().len();
        panic!(
            "managed scheduler stalled: phase={phase}, observed={}, active=[{}, {}]",
            observed,
            probe.active[0].load(std::sync::atomic::Ordering::SeqCst),
            probe.active[1].load(std::sync::atomic::Ordering::SeqCst),
        )
    });
}

#[derive(Clone)]
struct DeniedObservationPort {
    owner: String,
    admissions: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait::async_trait]
impl astra_memoria::MemoriaPort for DeniedObservationPort {
    fn bind_owner(&self, owner: &str) -> Result<Arc<dyn astra_memoria::MemoriaPort>, String> {
        assert_eq!(owner, self.owner);
        Ok(Arc::new(self.clone()))
    }
    async fn admits_operation(&self, write: bool) -> Result<bool, String> {
        assert!(write);
        self.admissions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(false)
    }
    async fn retrieve_ext(
        &self,
        _: &str,
        _: Option<&str>,
        _: usize,
        _: bool,
    ) -> Result<Vec<astra_memoria::MemoriaMemory>, astra_memoria::MemoriaOperationError> {
        panic!("denied observation must not retrieve")
    }
    async fn store(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<String, String> {
        panic!("denied observation must not write")
    }
    async fn purge_working(&self, _: &str) -> Result<u64, String> {
        panic!("denied observation must not purge")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn denied_recovery_never_reads_manifest_payloads() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .unwrap();
    let b = bootstrap().await;
    let session = uuid::Uuid::new_v4().to_string();
    seed_observation_sessions(&b.ctx, std::slice::from_ref(&session)).await;
    let admissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = b.ctx.app_state.clone().with_turn_observer_worker(Arc::new(
        astra_runtime::DatabaseTurnObserverWorker::new(Some(Arc::new(DeniedObservationPort {
            owner: b.ctx.user_id.clone(),
            admissions: admissions.clone(),
        }))),
    ));
    let owner = astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
        &state,
        &b.ctx.user_id,
        &session,
    )
    .await
    .unwrap();
    // Exercise deferred eligibility deliberately, even on databases where a
    // just-committed NOW(6) marker is immediately visible to the next claim.
    sqlx::query("UPDATE session_context_heads SET observation_due_at = DATE_ADD(NOW(6), INTERVAL 2 SECOND) WHERE isolation_domain='server' AND owner_user_id=? AND session_id=? AND branch_id='main'")
        .bind(&b.ctx.user_id).bind(&session).execute(&b.ctx.pool).await.unwrap();
    *capture.0.lock().unwrap() = Some(Vec::new());
    let history = astra_core::history_work::HistoryWorkScenario::begin("denied-recovery").unwrap();
    drive_scoped_observation_when_due(&owner).await.unwrap();
    let samples = capture.0.lock().unwrap().take().unwrap();
    let report = history.finish().unwrap();
    assert_eq!(admissions.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        samples
            .iter()
            .all(|sample| !sample.sql.contains("conversation_manifest_nodes")
                && !sample.sql.contains("session_forks")),
        "denied/read-only recovery must stop before canonical payload loading"
    );
    assert_eq!(
        report
            .scoped
            .measurement(astra_core::history_work::HistoryWorkSite::MemoryExtractionQueue)
            .events,
        0
    );
    assert!(
        state
            .metrics_registry()
            .render_prometheus()
            .contains("outcome=\"access_disabled\"} 1")
    );
    assert!(!owner.drive_one().await.unwrap());
    b.ctx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn observer_expiry_during_dispatch_does_not_acknowledge_progress() {
    require_system_e2e_env();
    let b = bootstrap().await;
    let session = uuid::Uuid::new_v4().to_string();
    seed_observation_sessions(&b.ctx, std::slice::from_ref(&session)).await;
    let worker = Arc::new(HeldCanonicalObserver {
        entered: Default::default(),
        permits: tokio::sync::Semaphore::new(0),
        seen: Default::default(),
        fail_once: std::sync::atomic::AtomicBool::new(false),
        failed_session: Default::default(),
    });
    let state = b
        .ctx
        .app_state
        .clone()
        .with_turn_observer_worker(worker.clone());
    let owner = Arc::new(
        astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
            &state,
            &b.ctx.user_id,
            &session,
        )
        .await
        .unwrap(),
    );
    let active = owner.clone();
    let attempt = tokio::spawn(async move { drive_scoped_observation_when_due(&active).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        worker.entered.notified(),
    )
    .await
    .unwrap();
    owner.expire_lease().await;
    worker.permits.add_permits(1);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .unwrap()
        .unwrap();
    assert!(result.unwrap_err().contains("checkpoint"));
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_context_heads WHERE owner_user_id=? AND session_id=? AND observation_json IS NOT NULL",
    )
    .bind(&b.ctx.user_id)
    .bind(&session)
    .fetch_one(&b.ctx.pool)
    .await
    .unwrap();
    assert_eq!(
        rows, 0,
        "expired in-flight work has no acknowledged watermark"
    );
    let restarted = astra_runtime::server::provider_test_support::ObservationRecoveryForTest::new(
        &state,
        &b.ctx.user_id,
        &session,
    )
    .await
    .unwrap();
    worker.permits.add_permits(1);
    drive_scoped_observation_when_due(&restarted).await.unwrap();
    assert!(!restarted.drive_one().await.unwrap());
    b.ctx.close().await;
}
