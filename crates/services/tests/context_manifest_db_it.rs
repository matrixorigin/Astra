//! Live MatrixOne regression tests for context-manifest transactional writes.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test context_manifest_db_it -- --ignored --nocapture --test-threads=1
//! ```

use astra_core::SharedPool;
use astra_services::{
    ContextManifestError, ContextManifestItemWrite, ContextManifestWrite,
    DatabaseContextManifestStore,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn manifest_replay_and_collision_preserve_original_and_tenant_identity() {
    use astra_services::observation_capture::DurableCaptureOutcome;
    let pool = common::setup_pool().await;
    let user = common::id("capture-user");
    let other_user = common::id("capture-other-user");
    let session = common::id("capture-session");
    let other_session = common::id("capture-other-session");
    let manifest_id = common::id("shared-manifest");
    insert_session(&pool, &user, &session).await;
    insert_session(&pool, &other_user, &other_session).await;
    let store = DatabaseContextManifestStore::new(pool.clone());
    let mut original = manifest(&manifest_id, &user, &session, None);
    original.reason = "unknown-reason-for-test".into();
    let mut dropped = item(&session, 1);
    dropped.included = false;
    dropped.reason = "budget_exceeded".into();
    let items = vec![dropped, item(&session, 0)];
    assert_eq!(
        store
            .save_manifest(original.clone(), items.clone())
            .await
            .unwrap(),
        DurableCaptureOutcome::Inserted
    );
    let mut reordered = items.clone();
    reordered.reverse();
    assert_eq!(
        store
            .save_manifest(original.clone(), reordered)
            .await
            .unwrap(),
        DurableCaptureOutcome::Replayed
    );
    let mut changed = items.clone();
    changed[0].source_id = "different-source".into();
    assert!(matches!(
        store.save_manifest(original, changed).await.unwrap(),
        DurableCaptureOutcome::Collision { .. }
    ));
    assert_eq!(
        store
            .save_manifest(
                manifest(&manifest_id, &other_user, &other_session, None),
                vec![item(&other_session, 0)]
            )
            .await
            .unwrap(),
        DurableCaptureOutcome::Inserted
    );

    let original_items: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM context_manifest_items WHERE user_id = ? AND manifest_id = ?",
    )
    .bind(&user)
    .bind(&manifest_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(original_items, 2);
    let persisted = sqlx::query(
        "SELECT item_order, included, reason FROM context_manifest_items
         WHERE user_id = ? AND manifest_id = ? ORDER BY item_order",
    )
    .bind(&user)
    .bind(&manifest_id)
    .fetch_all(pool.get())
    .await
    .unwrap();
    assert_eq!(persisted[0].try_get::<i16, _>("included").unwrap(), 1);
    assert_eq!(persisted[1].try_get::<i16, _>("included").unwrap(), 0);
    assert_eq!(
        persisted[1].try_get::<String, _>("reason").unwrap(),
        "budget_exceeded"
    );
    let dropped_count: i32 = sqlx::query_scalar(
        "SELECT dropped_count FROM context_manifests WHERE user_id = ? AND manifest_id = ?",
    )
    .bind(&user)
    .bind(&manifest_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        dropped_count, 1,
        "replay and collision preserve computed dropped count"
    );
    let changed_items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM context_manifest_items WHERE user_id = ? AND manifest_id = ? AND source_id = 'different-source'")
        .bind(&user).bind(&manifest_id).fetch_one(pool.get()).await.unwrap();
    assert_eq!(changed_items, 0);
    let other_items: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM context_manifest_items WHERE user_id = ? AND manifest_id = ?",
    )
    .bind(&other_user)
    .bind(&manifest_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(other_items, 1);
    let diagnostics: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'manifest.reason_unknown'")
        .bind(&user).bind(&session).fetch_one(pool.get()).await.unwrap();
    assert_eq!(
        diagnostics, 1,
        "replay and collision must not repeat derived diagnostics"
    );
    let collisions: u64 = sqlx::query_scalar("SELECT collision_count FROM observation_identity_collisions WHERE user_id = ? AND identity_kind = 'context_manifest' AND identity_id = ?")
        .bind(&user).bind(&manifest_id).fetch_one(pool.get()).await.unwrap();
    assert_eq!(collisions, 1);
    use astra_services::auth::session::{DatabaseSessionService, SessionService};
    DatabaseSessionService::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone())
        .delete_session(session.clone(), user.clone())
        .await
        .expect("delete only the first owner's session");
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM context_manifest_items WHERE user_id = ? AND manifest_id = ?",
    )
    .bind(&other_user)
    .bind(&manifest_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        remaining, 1,
        "deleting equal manifest ID must preserve the other tenant"
    );
    let deleted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM context_manifest_items WHERE user_id = ? AND manifest_id = ?",
    )
    .bind(&user)
    .bind(&manifest_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(deleted, 0);
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn manifest_replays_reuse_their_physical_connection() {
    use astra_services::observation_capture::DurableCaptureOutcome;

    let (_, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_min_connections = 1;
    settings.db_pool_max_connections = 1;
    let pool = SharedPool::new(&settings)
        .await
        .expect("create one-connection MatrixOne pool");
    let user_id = common::id("replay-connection-user");
    let session_id = common::id("replay-connection-session");
    let manifest_id = common::id("replay-connection-manifest");
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseContextManifestStore::new(pool.clone());
    let header = manifest(&manifest_id, &user_id, &session_id, None);
    let items = vec![item(&session_id, 0)];

    assert_eq!(
        store
            .save_manifest(header.clone(), items.clone())
            .await
            .expect("insert manifest"),
        DurableCaptureOutcome::Inserted
    );
    let connection_id_before: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool.get())
        .await
        .expect("read connection ID before replay");

    for replay in 1..=2 {
        assert_eq!(
            store
                .save_manifest(header.clone(), items.clone())
                .await
                .expect("replay manifest"),
            DurableCaptureOutcome::Replayed
        );
        let connection_id_after: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(pool.get())
            .await
            .expect("read connection ID after replay");
        assert_eq!(
            connection_id_after, connection_id_before,
            "successful replay {replay} must return the physical connection to the pool"
        );
    }
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn cross_session_artifact_references_replay_once_with_owner_isolation() {
    use astra_services::observation_capture::DurableCaptureOutcome;
    let pool = common::setup_pool().await;
    let owner = common::id("ref-owner");
    let foreign = common::id("ref-foreign");
    let target_session = common::id("ref-target");
    let source_session = common::id("ref-source");
    let artifact_id = common::id("ref-artifact");
    insert_session(&pool, &owner, &target_session).await;
    insert_session(&pool, &owner, &source_session).await;
    insert_session(&pool, &foreign, &source_session).await;
    for user in [&owner, &foreign] {
        sqlx::query(
            "INSERT INTO session_artifacts
             (user_id, session_id, artifact_id, artifact_kind, content_json,
              referenced_by_manifest_count) VALUES (?, ?, ?, 'test', '{}', 0)",
        )
        .bind(user)
        .bind(&source_session)
        .bind(&artifact_id)
        .execute(pool.get())
        .await
        .unwrap();
    }
    let header = manifest(&common::id("cross-ref"), &owner, &target_session, None);
    let mut reference = item(&source_session, 0);
    reference.source_table = "session_artifacts".into();
    reference.source_id = artifact_id.clone();
    let store = DatabaseContextManifestStore::new(pool.clone());
    assert_eq!(
        store
            .save_manifest(header.clone(), vec![reference.clone()])
            .await
            .unwrap(),
        DurableCaptureOutcome::Inserted
    );
    assert_eq!(
        store.save_manifest(header, vec![reference]).await.unwrap(),
        DurableCaptureOutcome::Replayed
    );
    for (user, expected) in [(&owner, 1_i64), (&foreign, 0_i64)] {
        let count: i64 = sqlx::query_scalar(
            "SELECT referenced_by_manifest_count FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(user)
        .bind(&source_session)
        .bind(&artifact_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(
            count, expected,
            "only the manifest owner's source artifact is referenced once"
        );
    }
    sqlx::query(
        "UPDATE session_artifacts SET status = 'expired', metadata = ?
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(json!({"summary": "persisted owner summary"}).to_string())
    .bind(&owner)
    .bind(&source_session)
    .bind(&artifact_id)
    .execute(pool.get())
    .await
    .unwrap();
    let rendered = store
        .render_artifact_manifest_item(&owner, &source_session, &artifact_id, None)
        .await
        .unwrap();
    assert!(rendered.contains("historical, raw no longer available, summary preserved"));
    assert!(rendered.contains("persisted owner summary"));
    let foreign_rendered = store
        .render_artifact_manifest_item(&foreign, &source_session, &artifact_id, None)
        .await
        .unwrap();
    assert_eq!(
        foreign_rendered, "{}",
        "equal artifact IDs must not expose another owner's summary or status"
    );
}

async fn insert_session(pool: &SharedPool, user_id: &str, session_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'manifest-db-it', 'manifest db integration', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .expect("insert active session");
}

fn manifest(
    manifest_id: &str,
    user_id: &str,
    session_id: &str,
    run_id: Option<&str>,
) -> ContextManifestWrite {
    ContextManifestWrite {
        manifest_id: manifest_id.to_string(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        run_id: run_id.map(str::to_string),
        turn_id: common::id("turn"),
        model_provider: "test".to_string(),
        model_name: "manifest-db-it".to_string(),
        context_window_tokens: 8_000,
        max_output_tokens: 512,
        total_estimated_tokens: 1_024,
        policy_version: "context_manifest_v1".to_string(),
        tokenizer_id: None,
        budget_template_id: None,
        turn_intent: None,
        reason: "normal_turn".to_string(),
        manifest_json: json!({"test": "context_manifest_db_it"}),
    }
}

fn item(session_id: &str, item_order: i32) -> ContextManifestItemWrite {
    ContextManifestItemWrite {
        session_id: session_id.to_string(),
        item_order,
        zone: "recent_tail".to_string(),
        source_table: "runtime_messages".to_string(),
        source_id: format!("message-{item_order}"),
        source_hash: None,
        included: true,
        token_estimate: 8,
        budget_tokens: 16,
        reason: "normal_turn".to_string(),
        render_mode: "plain_text".to_string(),
        raw_ref: None,
    }
}

fn assert_duplicate_item_insert(error: ContextManifestError) {
    match error {
        ContextManifestError::Database {
            operation: "insert_context_manifest_items",
            source,
            ..
        } => {
            let database_error = source
                .as_database_error()
                .expect("duplicate item order must be a database error");
            assert_eq!(
                database_error.code().as_deref(),
                Some("23000"),
                "expected integrity-constraint SQLSTATE"
            );
            assert!(
                database_error
                    .message()
                    .to_ascii_lowercase()
                    .contains("duplicate"),
                "expected duplicate-key message, got {}",
                database_error.message()
            );
        }
        other => panic!("expected duplicate item insert failure, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn later_item_batch_failure_rolls_back_manifest_and_all_items() {
    let pool = common::setup_pool().await;
    let user_id = common::id("manifest-rollback-user");
    let session_id = common::id("manifest-rollback-session");
    let manifest_id = common::id("manifest-rollback");
    insert_session(&pool, &user_id, &session_id).await;

    let mut items = (0..129)
        .map(|order| item(&session_id, order))
        .collect::<Vec<_>>();
    // After sorting, the duplicate stays at positions 127/128 across the
    // SQL batch boundary, so batch two fails after batch one succeeded.
    items[128].item_order = 127;

    let store = DatabaseContextManifestStore::new(pool.clone());
    let control_id = common::id("manifest-multibatch-control");
    store
        .save_manifest(
            manifest(&control_id, &user_id, &session_id, None),
            (0..129).map(|order| item(&session_id, order)).collect(),
        )
        .await
        .expect("129-item control must cross the batch boundary successfully");
    let control_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_manifest_items WHERE manifest_id = ?")
            .bind(&control_id)
            .fetch_one(pool.get())
            .await
            .expect("count multi-batch control items");
    assert_eq!(control_count, 129);

    let error = store
        .save_manifest(manifest(&manifest_id, &user_id, &session_id, None), items)
        .await
        .expect_err("duplicate item order must fail the write");
    assert_duplicate_item_insert(error);

    let manifest_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_manifests WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .expect("count rolled-back manifest");
    let item_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_manifest_items WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .expect("count rolled-back manifest items");
    assert_eq!((manifest_count, item_count), (0, 0));
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn artifact_reference_updates_are_exact_and_roll_back_after_later_update_failure() {
    let pool = common::setup_pool().await;
    let user_id = common::id("artifact-rollback-user");
    let session_id = common::id("artifact-rollback-session");
    let manifest_id = common::id("artifact-rollback-manifest");
    let first_artifact_id = format!("a-{}", Uuid::new_v4().simple());
    let failing_artifact_id = format!("z-{}", Uuid::new_v4().simple());
    let unaffected_artifact_id = common::id("unaffected-artifact");
    insert_session(&pool, &user_id, &session_id).await;
    for (artifact_id, reference_count) in [
        (&first_artifact_id, 7_i64),
        (&failing_artifact_id, i32::MAX as i64),
        (&unaffected_artifact_id, 11_i64),
    ] {
        sqlx::query(
            "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, artifact_kind, content_json, metadata,
          retention_policy, status, referenced_by_manifest_count, created_at, updated_at)
         VALUES (?, ?, ?, 'test', '{}', '{}', 'default', 'active', ?, NOW(6), NOW(6))",
        )
        .bind(artifact_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(reference_count)
        .execute(pool.get())
        .await
        .expect("insert artifact");
    }

    let control_id = common::id("artifact-reference-control");
    let mut control_items = vec![
        item(&session_id, 0),
        item(&session_id, 1),
        item(&session_id, 2),
    ];
    for value in &mut control_items[..2] {
        value.source_table = "session_artifacts".to_string();
        value.source_id = first_artifact_id.clone();
    }
    control_items[2].source_table = "session_artifacts".to_string();
    control_items[2].source_id = unaffected_artifact_id.clone();
    let control_manifest = manifest(&control_id, &user_id, &session_id, None);
    DatabaseContextManifestStore::new(pool.clone())
        .save_manifest(control_manifest.clone(), control_items.clone())
        .await
        .expect("successful artifact references must commit");
    assert_eq!(
        DatabaseContextManifestStore::new(pool.clone())
            .save_manifest(control_manifest.clone(), control_items.clone())
            .await
            .expect("retry after a lost commit acknowledgement must replay"),
        astra_services::observation_capture::DurableCaptureOutcome::Replayed,
    );
    control_items[0].source_id = unaffected_artifact_id.clone();
    assert!(matches!(
        DatabaseContextManifestStore::new(pool.clone())
            .save_manifest(control_manifest, control_items)
            .await
            .expect("identity conflict must be a typed outcome"),
        astra_services::observation_capture::DurableCaptureOutcome::Collision { .. }
    ));
    for (artifact_id, expected) in [
        (&first_artifact_id, 9_i64),
        (&unaffected_artifact_id, 12_i64),
    ] {
        let actual: i64 = sqlx::query_scalar(
            "SELECT referenced_by_manifest_count FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(artifact_id)
        .fetch_one(pool.get())
        .await
        .expect("load committed artifact reference count");
        assert_eq!(
            actual, expected,
            "committed references must preserve multiplicity"
        );
    }

    let items = (0..129)
        .map(|order| {
            let mut value = item(&session_id, order);
            value.source_table = "session_artifacts".to_string();
            value.source_id = if order < 128 {
                first_artifact_id.clone()
            } else {
                failing_artifact_id.clone()
            };
            value
        })
        .collect::<Vec<_>>();

    let error = DatabaseContextManifestStore::new(pool.clone())
        .save_manifest(manifest(&manifest_id, &user_id, &session_id, None), items)
        .await
        .expect_err("overflow in the second artifact update must fail the transaction");
    match error {
        ContextManifestError::Database {
            operation: "increment_manifest_artifact_ref",
            entity,
            ..
        } => assert_eq!(entity, failing_artifact_id),
        other => panic!("expected artifact-reference update failure, got {other:?}"),
    }

    for (artifact_id, expected) in [
        (&first_artifact_id, 9_i64),
        (&failing_artifact_id, i32::MAX as i64),
        (&unaffected_artifact_id, 12_i64),
    ] {
        let actual: i64 = sqlx::query_scalar(
            "SELECT referenced_by_manifest_count FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(artifact_id)
        .fetch_one(pool.get())
        .await
        .expect("load artifact reference count");
        assert_eq!(
            actual, expected,
            "failed transaction must restore {artifact_id}"
        );
    }
    let manifest_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_manifests WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .expect("count rolled-back manifest");
    let item_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_manifest_items WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .expect("count rolled-back items");
    assert_eq!((manifest_count, item_count), (0, 0));
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn one_connection_preserves_distinct_nullable_parameter_shapes() {
    let (_, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_min_connections = 1;
    settings.db_pool_max_connections = 1;
    let pool = SharedPool::new(&settings)
        .await
        .expect("create one-connection MatrixOne pool");
    let user_id = common::id("nullable-user");
    let session_id = common::id("nullable-session");
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseContextManifestStore::new(pool.clone());

    let make_items = |values: [(Option<&str>, Option<&str>); 3]| {
        values
            .into_iter()
            .enumerate()
            .map(|(order, (source_hash, raw_ref))| {
                let mut item = item(&session_id, order as i32);
                item.source_hash = source_hash.map(str::to_string);
                item.raw_ref = raw_ref.map(str::to_string);
                item
            })
            .collect::<Vec<_>>()
    };

    let second_id = common::id("nullable-raw-ref-change");
    let mut second = manifest(&second_id, &user_id, &session_id, Some("run-present"));
    second.tokenizer_id = Some("tokenizer-present".to_string());
    second.budget_template_id = Some("budget-present".to_string());
    second.turn_intent = Some("intent-present".to_string());
    let third_id = common::id("nullable-source-hash-change");
    let mut third = manifest(&third_id, &user_id, &session_id, Some("run-third"));
    third.tokenizer_id = Some("tokenizer-third".to_string());
    third.budget_template_id = Some("budget-third".to_string());
    third.turn_intent = Some("intent-third".to_string());
    let cases = vec![
        (
            manifest(
                &common::id("nullable-all-none"),
                &user_id,
                &session_id,
                None,
            ),
            make_items([
                (None, None),
                (Some("first-hash-only"), None),
                (None, Some("conversation_log://nullable/first-ref-only")),
            ]),
            vec![None, None, None, None],
            vec![
                (None, None),
                (Some("first-hash-only"), None),
                (None, Some("conversation_log://nullable/first-ref-only")),
            ],
        ),
        (
            second,
            make_items([
                (None, None),
                (
                    Some("first-hash-only"),
                    Some("conversation_log://nullable/second-ref-only"),
                ),
                (None, None),
            ]),
            vec![
                Some("run-present"),
                Some("tokenizer-present"),
                Some("budget-present"),
                Some("intent-present"),
            ],
            vec![
                (None, None),
                (
                    Some("first-hash-only"),
                    Some("conversation_log://nullable/second-ref-only"),
                ),
                (None, None),
            ],
        ),
        (
            third,
            make_items([
                (None, None),
                (None, Some("conversation_log://nullable/second-ref-only")),
                (Some("third-hash-only"), None),
            ]),
            vec![
                Some("run-third"),
                Some("tokenizer-third"),
                Some("budget-third"),
                Some("intent-third"),
            ],
            vec![
                (None, None),
                (None, Some("conversation_log://nullable/second-ref-only")),
                (Some("third-hash-only"), None),
            ],
        ),
    ];

    for (manifest, items, expected_header, expected_items) in cases {
        let manifest_id = manifest.manifest_id.clone();
        store
            .save_manifest(manifest, items)
            .await
            .expect("save nullable shape on the same connection");

        let header = sqlx::query(
            "SELECT run_id, tokenizer_id, budget_template_id, turn_intent
             FROM context_manifests WHERE manifest_id = ?",
        )
        .bind(&manifest_id)
        .fetch_one(pool.get())
        .await
        .expect("load nullable manifest header");
        let actual_header = [
            "run_id",
            "tokenizer_id",
            "budget_template_id",
            "turn_intent",
        ]
        .into_iter()
        .map(|column| header.try_get::<Option<String>, _>(column).unwrap())
        .collect::<Vec<_>>();
        let expected_header = expected_header
            .into_iter()
            .map(|value| value.map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(actual_header, expected_header);

        let rows = sqlx::query(
            "SELECT source_hash, raw_ref FROM context_manifest_items
             WHERE manifest_id = ? ORDER BY item_order",
        )
        .bind(&manifest_id)
        .fetch_all(pool.get())
        .await
        .expect("load mixed nullable item shapes");
        let actual = rows
            .iter()
            .map(|row| {
                (
                    row.try_get::<Option<String>, _>("source_hash").unwrap(),
                    row.try_get::<Option<String>, _>("raw_ref").unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let expected = expected_items
            .into_iter()
            .map(|(hash, raw_ref)| (hash.map(str::to_string), raw_ref.map(str::to_string)))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn pending_delete_fence_rejects_manifest_without_partial_rows() {
    let pool = common::setup_pool().await;
    let user_id = common::id("deleting-user");
    let session_id = common::id("deleting-session");
    let manifest_id = common::id("deleting-manifest");
    insert_session(&pool, &user_id, &session_id).await;
    let fence = sqlx::query(
        "INSERT INTO agent_session_lifecycle_fences
         (session_id, user_id, delete_requested_at, created_at, updated_at)
         VALUES (?, ?, NOW(6), NOW(6), NOW(6))",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(pool.get())
    .await
    .expect("insert pending-delete session fence");
    assert_eq!(fence.rows_affected(), 1, "pending-delete fence must exist");

    let result = DatabaseContextManifestStore::new(pool.clone())
        .save_manifest(
            manifest(&manifest_id, &user_id, &session_id, None),
            vec![item(&session_id, 0)],
        )
        .await;
    assert!(
        result.is_err(),
        "pending delete must reject a manifest write"
    );

    let manifest_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_manifests WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .expect("count rejected manifest");
    let item_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_manifest_items WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .expect("count rejected manifest items");
    assert_eq!((manifest_count, item_count), (0, 0));
}
