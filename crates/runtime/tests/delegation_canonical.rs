//! Public RunEngine delegation facts have one durable run/event owner.
mod test_support;

use std::{collections::BTreeSet, sync::Arc};

use astra_runtime::server::run::engine::RunEngine;
use astra_services::runs::AtomicExecutionOwnerCancellation;
use astra_services::{DatabaseRunStateStore, InMemoryRunStateStore};
use astra_turn_core::orchestration_types::CancellationOrigin;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

async fn exercise_delegation_lifecycle(engine: &RunEngine, user: &str, session: &str, root: &str) {
    let child = format!("child-{root}");
    let grandchild = format!("grandchild-{root}");
    let retry = format!("retry-{root}");
    let delegation = format!("delegation-{root}");
    let nested_delegation = format!("nested-{root}");
    engine.start_run(root, user, session).await.unwrap();
    let child_owner = engine
        .start_run_ext(
            &child,
            user,
            session,
            Some(root),
            Some(&delegation),
            Some("coder"),
            None,
        )
        .await
        .unwrap();
    let grandchild_owner = engine
        .start_run_ext(
            &grandchild,
            user,
            session,
            Some(&child),
            Some(&nested_delegation),
            Some("reviewer"),
            None,
        )
        .await
        .unwrap();

    for (bad_user, bad_session) in [("foreign-owner", session), (user, "foreign-session")] {
        let rejected = format!("rejected-{bad_user}-{bad_session}-{root}");
        assert!(
            engine
                .start_run_ext(
                    &rejected,
                    bad_user,
                    bad_session,
                    Some(&child),
                    Some(&nested_delegation),
                    Some("reviewer"),
                    None,
                )
                .await
                .is_err()
        );
        assert!(
            engine
                .load_run(bad_user, &rejected)
                .await
                .unwrap()
                .is_none()
        );
    }
    let before = engine.load_run(user, &child).await.unwrap().unwrap();
    for (owner, scope, generation) in [
        ("foreign-owner", session, child_owner.owner_generation),
        (user, "foreign-session", child_owner.owner_generation),
        (user, session, child_owner.owner_generation + 1),
    ] {
        assert!(
            !engine
                .transition_status_with_events_if_current_owner(
                    owner,
                    scope,
                    &child,
                    &["running"],
                    generation,
                    "failed",
                    None,
                    Some("rejected"),
                    &[json!({"event_type":"run_error", "data":{"error":"rejected"}})],
                )
                .await
                .unwrap()
        );
    }
    assert_eq!(
        engine.load_run(user, &child).await.unwrap().unwrap(),
        before
    );
    assert!(
        engine
            .load_run("foreign-owner", &child)
            .await
            .unwrap()
            .is_none()
    );

    assert!(
        engine
            .persist_usage_if_current_owner(
                user,
                session,
                &child,
                child_owner.owner_generation,
                17,
                9,
                2,
            )
            .await
            .unwrap()
    );
    assert!(
        engine
            .transition_status_with_events_if_current_owner(
                user,
                session,
                &child,
                &["running"],
                child_owner.owner_generation,
                "waiting",
                Some("review_ready"),
                None,
                &[json!({"event_type":"child_waiting"})],
            )
            .await
            .unwrap()
    );
    let waiting = engine.load_run(user, &child).await.unwrap().unwrap();
    assert_eq!(waiting.status, "waiting");
    assert_eq!(waiting.waiting_for.as_deref(), Some("review_ready"));
    assert_eq!(
        waiting.events.last().unwrap()["event_type"],
        "child_waiting"
    );

    assert!(
        engine
            .persist_delegation_outcome_status_if_current_owner(
                user,
                session,
                &grandchild,
                grandchild_owner.owner_generation,
                "completed",
                None,
                None,
                &[],
            )
            .await
            .unwrap()
    );
    assert!(
        engine
            .persist_delegation_outcome_status_if_current_owner(
                user,
                session,
                &child,
                child_owner.owner_generation,
                "failed",
                None,
                Some("review blocked"),
                &[],
            )
            .await
            .unwrap()
    );
    let failed = engine.load_run(user, &child).await.unwrap().unwrap();
    assert_eq!(failed.error_message.as_deref(), Some("review blocked"));
    assert!(failed.waiting_for.is_none());
    assert_eq!(
        (
            failed.total_prompt_tokens,
            failed.total_completion_tokens,
            failed.total_tool_calls
        ),
        (17, 9, 2)
    );
    assert_eq!(
        failed
            .events
            .iter()
            .map(|event| event["event_type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["run_started", "child_waiting", "run_error", "run_finished"]
    );
    assert!(
        !engine
            .persist_delegation_outcome_status_if_current_owner(
                user,
                session,
                &child,
                child_owner.owner_generation,
                "failed",
                None,
                Some("late duplicate"),
                &[],
            )
            .await
            .unwrap()
    );
    assert_eq!(
        engine.load_run(user, &child).await.unwrap().unwrap(),
        failed
    );

    let retry_owner = engine
        .start_run_ext(
            &retry,
            user,
            session,
            Some(root),
            Some(&delegation),
            Some("coder"),
            Some(&child),
        )
        .await
        .unwrap();
    assert!(
        engine
            .transition_status_with_events_if_current_owner(
                user,
                session,
                &retry,
                &["running"],
                retry_owner.owner_generation,
                "paused",
                Some("user_resume"),
                None,
                &[json!({"event_type":"run_paused"})],
            )
            .await
            .unwrap()
    );
    let paused = engine.load_run(user, &retry).await.unwrap().unwrap();
    assert!(
        !engine
            .persist_delegation_outcome_status_if_current_owner(
                user,
                session,
                &retry,
                retry_owner.owner_generation,
                "completed",
                None,
                None,
                &[],
            )
            .await
            .unwrap()
    );
    assert_eq!(
        engine.load_run(user, &retry).await.unwrap().unwrap(),
        paused
    );
    assert_eq!(
        engine
            .cancel_if_exact_live_owner(
                user,
                session,
                &retry,
                retry_owner.owner_generation,
                &["paused"],
                CancellationOrigin::Runtime,
                "retry cancelled by runtime",
            )
            .await
            .unwrap(),
        AtomicExecutionOwnerCancellation::Committed
    );
    let cancelled = engine.load_run(user, &retry).await.unwrap().unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(cancelled.retry_of.as_deref(), Some(child.as_str()));
    assert_eq!(
        cancelled.events.last().unwrap()["data"]["cancellation_origin"],
        "runtime"
    );
    assert!(
        !engine
            .persist_delegation_outcome_status_if_current_owner(
                user,
                session,
                &retry,
                retry_owner.owner_generation,
                "completed",
                None,
                None,
                &[],
            )
            .await
            .unwrap()
    );
    assert_eq!(
        engine.load_run(user, &retry).await.unwrap().unwrap(),
        cancelled
    );

    for (run_id, parent, path, depth, delegation_id, agent, status) in [
        (root, None, root.to_string(), 0, None, None, "running"),
        (
            child.as_str(),
            Some(root),
            format!("{root}/{child}"),
            1,
            Some(delegation.as_str()),
            Some("coder"),
            "failed",
        ),
        (
            grandchild.as_str(),
            Some(child.as_str()),
            format!("{root}/{child}/{grandchild}"),
            2,
            Some(nested_delegation.as_str()),
            Some("reviewer"),
            "completed",
        ),
        (
            retry.as_str(),
            Some(root),
            format!("{root}/{retry}"),
            1,
            Some(delegation.as_str()),
            Some("coder"),
            "cancelled",
        ),
    ] {
        let run = engine.load_run(user, run_id).await.unwrap().unwrap();
        assert_eq!(run.user_id, user);
        assert_eq!(run.session_id, session);
        assert_eq!(run.parent_run_id.as_deref(), parent);
        assert_eq!(run.root_run_id.as_deref(), Some(root));
        assert_eq!(run.ancestor_path.as_deref(), Some(path.as_str()));
        assert_eq!(run.depth, depth);
        assert_eq!(run.delegation_id.as_deref(), delegation_id);
        assert_eq!(run.agent_id.as_deref(), agent);
        assert_eq!(run.retry_scope.as_deref(), Some("node"));
        assert_eq!(run.status, status);
        assert_eq!(run.events[0]["event_type"], "run_started");
        let control = engine
            .load_run_control(user, run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(control.status, run.status);
        assert_eq!(control.ancestor_path, run.ancestor_path);
        assert_eq!(control.run_generation, run.run_generation);
        let display = engine
            .load_run_projection(user, run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(display.status, run.status);
        assert_eq!(display.error_message, run.error_message);
        assert_eq!(display.projection_event_idx, run.last_event_idx);
    }
    let listed = engine.list_session_runs(user, session, 10).await.unwrap();
    assert!(!listed.truncated);
    assert_eq!(
        listed
            .runs
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([root, child.as_str(), grandchild.as_str(), retry.as_str()])
    );
    assert!(
        engine
            .list_session_runs("foreign-owner", session, 10)
            .await
            .unwrap()
            .runs
            .is_empty()
    );
    let siblings = engine.find_sub_runs(user, &delegation).await.unwrap();
    assert_eq!(
        siblings
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([child.as_str(), retry.as_str()])
    );
    assert!(
        engine
            .find_sub_runs("foreign-owner", &delegation)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn public_run_engine_preserves_canonical_delegation_tree_and_lifecycle() {
    let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    exercise_delegation_lifecycle(&engine, "owner", "session", "root").await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and MatrixOne"]
async fn public_run_engine_persists_canonical_delegation_facts_on_matrixone() {
    let settings = test_support::require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    astra_services::ensure_core_schema(&settings, &catalog)
        .await
        .unwrap();
    let pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let user = format!("owner-{suffix}");
    let session = format!("session-{suffix}");
    let root = format!("root-{suffix}");
    sqlx::query("INSERT INTO agent_sessions (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at) VALUES (?, ?, 'canonical-test', 'canonical delegation', 'active', '{}', NOW(6), NOW(6))")
        .bind(&session).bind(&user).execute(pool.get()).await.unwrap();
    let engine = RunEngine::new(Arc::new(
        DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id(format!("canonical-{suffix}")),
    ));
    exercise_delegation_lifecycle(&engine, &user, &session, &root).await;
    let rows = sqlx::query("SELECT run_id, status, retry_of, ancestor_path, depth FROM agent_runs WHERE user_id = ? AND session_id = ?")
        .bind(&user).bind(&session).fetch_all(pool.get()).await.unwrap();
    assert_eq!(rows.len(), 4);
    let child = format!("child-{root}");
    let grandchild = format!("grandchild-{root}");
    let retry = format!("retry-{root}");
    let retry_row = rows
        .iter()
        .find(|row| row.get::<String, _>("run_id") == retry)
        .unwrap();
    assert_eq!(retry_row.get::<String, _>("status"), "cancelled");
    assert_eq!(
        retry_row.get::<Option<String>, _>("retry_of").as_deref(),
        Some(child.as_str())
    );
    let nested_row = rows
        .iter()
        .find(|row| row.get::<String, _>("run_id") == grandchild)
        .unwrap();
    assert_eq!(
        nested_row.get::<String, _>("ancestor_path"),
        format!("{root}/{child}/{grandchild}")
    );
    assert_eq!(nested_row.get::<i64, _>("depth"), 2);
    let events = sqlx::query("SELECT event_type FROM agent_run_events WHERE user_id = ? AND run_id = ? ORDER BY event_idx")
        .bind(&user).bind(&child).fetch_all(pool.get()).await.unwrap();
    assert_eq!(
        events
            .iter()
            .map(|row| row.get::<String, _>("event_type"))
            .collect::<Vec<_>>(),
        ["run_started", "child_waiting", "run_error", "run_finished"]
    );
}
