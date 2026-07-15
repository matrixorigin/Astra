//! Live MatrixOne contract tests for durable semantic read observations.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-services \
//!   --test semantic_read_observation_store_db_it -- --ignored

mod common;

use std::collections::BTreeMap;

use astra_services::semantic_read_observation_store::{
    DatabaseSemanticReadObservationStore, SemanticReadObservationStoreError,
};
use astra_turn_types::{
    DurableToolReference, NativeToolId, ProviderBindingRef, ResolvedToolDescriptorRef,
    SemanticFreshnessFact, SemanticFreshnessScope, SemanticReadCacheKey, SemanticReadCacheLimits,
    SemanticReadCacheLookup, SemanticReadFreshnessContext, SemanticReadObservation, ToolIdentity,
    ToolInvocationDecision, ToolInvocationResultPayload, ToolInvocationTerminalOutcome,
};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

fn cache_key(label: &str) -> SemanticReadCacheKey {
    let freshness = SemanticReadFreshnessContext::new(
        "live-db-owner-scope",
        vec![
            SemanticFreshnessFact::new(
                SemanticFreshnessScope::Resource,
                &format!("resource-{label}"),
                "revision-1",
            )
            .expect("freshness fact"),
        ],
    )
    .expect("freshness context");
    let decision = ToolInvocationDecision::new(&json!({"policy": label})).expect("policy decision");
    SemanticReadCacheKey::new(
        DurableToolReference::Provider {
            descriptor: ResolvedToolDescriptorRef::new(
                ToolIdentity::new(
                    ProviderBindingRef::new("live-db-binding").expect("binding"),
                    NativeToolId::new("read").expect("native tool"),
                ),
                "descriptor-v1",
            )
            .expect("descriptor"),
        },
        &json!({"query": label}),
        &decision.decision_id,
        &freshness,
    )
    .expect("cache key")
}

fn observation(key: SemanticReadCacheKey, output: &str) -> SemanticReadObservation {
    SemanticReadObservation::from_terminal_outcome(
        key,
        &ToolInvocationTerminalOutcome::Succeeded {
            result: ToolInvocationResultPayload {
                output: output.to_string(),
                metadata: BTreeMap::new(),
                exit_semantics: None,
            },
        },
    )
    .expect("successful observation")
}

async fn seed_session(pool: &sqlx::MySqlPool, user_id: &str, session_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
         VALUES (?, ?, 'semantic-read-store-db-it', 'active', 0)",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert session");
}

async fn cleanup_session(pool: &sqlx::MySqlPool, user_id: &str, session_id: &str) {
    let _ =
        sqlx::query("DELETE FROM semantic_read_observations WHERE user_id = ? AND session_id = ?")
            .bind(user_id)
            .bind(session_id)
            .execute(pool)
            .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn durable_store_enforces_fill_fencing_bounds_isolation_and_corruption_removal() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let other_session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, &user_id, &session_id).await;
    cleanup_session(&pool, &user_id, &other_session_id).await;
    seed_session(&pool, &user_id, &session_id).await;
    seed_session(&pool, &user_id, &other_session_id).await;

    let store = DatabaseSemanticReadObservationStore::new(
        shared,
        SemanticReadCacheLimits {
            max_ready_entries: 2,
            max_ready_bytes: 1024 * 1024,
            max_in_flight_fills: 1,
        },
    )
    .expect("store");
    let first = cache_key("first");
    let second = cache_key("second");

    assert_eq!(
        store
            .lookup_or_claim(&user_id, &session_id, &first, "owner-a", 30_000)
            .await
            .expect("claim first"),
        SemanticReadCacheLookup::FillClaimed
    );
    assert!(matches!(
        store
            .lookup_or_claim(&user_id, &session_id, &first, "owner-b", 30_000)
            .await
            .expect("observe first fill"),
        SemanticReadCacheLookup::FillInProgress { .. }
    ));
    assert_eq!(
        store
            .lookup_or_claim(&user_id, &session_id, &second, "owner-b", 30_000)
            .await
            .expect("enforce fill bound"),
        SemanticReadCacheLookup::FillCapacityExceeded
    );
    assert!(matches!(
        store
            .complete_fill(
                &user_id,
                &session_id,
                &first,
                "owner-b",
                &observation(first.clone(), "wrong owner"),
            )
            .await,
        Err(SemanticReadObservationStoreError::FillOwnerOrLeaseMismatch)
    ));
    store
        .complete_fill(
            &user_id,
            &session_id,
            &first,
            "owner-a",
            &observation(first.clone(), "first result"),
        )
        .await
        .expect("complete owned fill");
    assert!(matches!(
        store
            .lookup_or_claim(&user_id, &session_id, &first, "reader", 30_000)
            .await
            .expect("read first"),
        SemanticReadCacheLookup::Hit(value) if value.result.output == "first result"
    ));

    store
        .lookup_or_claim(&user_id, &session_id, &second, "owner-b", 30_000)
        .await
        .expect("claim second");
    store
        .complete_fill(
            &user_id,
            &session_id,
            &second,
            "owner-b",
            &observation(second.clone(), "second result"),
        )
        .await
        .expect("complete second");
    sqlx::query(
        "UPDATE semantic_read_observations
         SET last_accessed_at = TIMESTAMPADD(SECOND, -2, CURRENT_TIMESTAMP(6))
         WHERE user_id = ? AND session_id = ? AND key_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&second.key_id)
    .execute(&pool)
    .await
    .expect("age second entry");

    let third = cache_key("third");
    store
        .lookup_or_claim(&user_id, &session_id, &third, "owner-c", 30_000)
        .await
        .expect("claim third");
    store
        .complete_fill(
            &user_id,
            &session_id,
            &third,
            "owner-c",
            &observation(third.clone(), "third result"),
        )
        .await
        .expect("complete third and evict LRU");
    let second_rows: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM semantic_read_observations
         WHERE user_id = ? AND session_id = ? AND key_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&second.key_id)
    .fetch_one(&pool)
    .await
    .expect("count evicted second entry");
    assert_eq!(second_rows, 0, "oldest ready observation must be evicted");

    let expiring = cache_key("expiring");
    assert_eq!(
        store
            .lookup_or_claim(&user_id, &session_id, &expiring, "stale", 30_000)
            .await
            .expect("claim expiring fill"),
        SemanticReadCacheLookup::FillClaimed
    );
    sqlx::query(
        "UPDATE semantic_read_observations
         SET fill_lease_expires_at = TIMESTAMPADD(SECOND, -1, CURRENT_TIMESTAMP(6))
         WHERE user_id = ? AND session_id = ? AND key_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&expiring.key_id)
    .execute(&pool)
    .await
    .expect("expire fill");
    assert_eq!(
        store
            .lookup_or_claim(&user_id, &session_id, &expiring, "current", 30_000)
            .await
            .expect("reclaim expired fill"),
        SemanticReadCacheLookup::FillClaimed
    );
    assert!(matches!(
        store
            .complete_fill(
                &user_id,
                &session_id,
                &expiring,
                "stale",
                &observation(expiring.clone(), "stale result"),
            )
            .await,
        Err(SemanticReadObservationStoreError::FillOwnerOrLeaseMismatch)
    ));
    store
        .complete_fill(
            &user_id,
            &session_id,
            &expiring,
            "current",
            &observation(expiring.clone(), "current result"),
        )
        .await
        .expect("complete reclaimed fill");

    sqlx::query(
        "UPDATE semantic_read_observations SET observation_bytes = observation_bytes + 1
         WHERE user_id = ? AND session_id = ? AND key_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&expiring.key_id)
    .execute(&pool)
    .await
    .expect("corrupt stored byte count");
    assert!(matches!(
        store
            .lookup_or_claim(&user_id, &session_id, &expiring, "reader", 30_000)
            .await,
        Err(SemanticReadObservationStoreError::ObservationSizeMismatchRemoved)
    ));
    let corrupted_rows: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM semantic_read_observations
         WHERE user_id = ? AND session_id = ? AND key_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&expiring.key_id)
    .fetch_one(&pool)
    .await
    .expect("count corrupt entry");
    assert_eq!(
        corrupted_rows, 0,
        "corrupt observations must fail closed and be removed"
    );

    assert_eq!(
        store
            .lookup_or_claim(&user_id, &other_session_id, &first, "other-session", 30_000,)
            .await
            .expect("session-isolated claim"),
        SemanticReadCacheLookup::FillClaimed
    );
    store
        .abandon_fill(&user_id, &other_session_id, &first, "other-session")
        .await
        .expect("abandon other-session fill");

    cleanup_session(&pool, &user_id, &session_id).await;
    cleanup_session(&pool, &user_id, &other_session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn concurrent_claims_across_store_instances_have_one_fill_owner() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, &user_id, &session_id).await;
    seed_session(&pool, &user_id, &session_id).await;
    let first = DatabaseSemanticReadObservationStore::new(
        shared.clone(),
        SemanticReadCacheLimits::default(),
    )
    .expect("first store");
    let second =
        DatabaseSemanticReadObservationStore::new(shared, SemanticReadCacheLimits::default())
            .expect("second store");
    let key = cache_key("concurrent");

    let (left, right) = tokio::join!(
        first.lookup_or_claim(&user_id, &session_id, &key, "left", 30_000),
        second.lookup_or_claim(&user_id, &session_id, &key, "right", 30_000),
    );
    let left = left.expect("left lookup");
    let right = right.expect("right lookup");
    assert!(matches!(
        (&left, &right),
        (
            SemanticReadCacheLookup::FillClaimed,
            SemanticReadCacheLookup::FillInProgress { .. }
        ) | (
            SemanticReadCacheLookup::FillInProgress { .. },
            SemanticReadCacheLookup::FillClaimed
        )
    ));
    let owner = if left == SemanticReadCacheLookup::FillClaimed {
        "left"
    } else {
        "right"
    };
    first
        .abandon_fill(&user_id, &session_id, &key, owner)
        .await
        .expect("owner abandons fill");
    cleanup_session(&pool, &user_id, &session_id).await;
}
