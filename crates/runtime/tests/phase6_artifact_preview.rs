mod test_support;

use astra_runtime::server::artifact_retention_sweeper::run_artifact_retention_gc_once;
use astra_services::{
    DatabaseRunStateStore, DatabaseSessionArtifactStore, DatabaseSessionService,
    DatabaseStateProjectionStore, SessionArtifactContentChunkV1,
    SessionArtifactContentDescriptorV1, SessionArtifactContentStore, SessionArtifactJsonRecord,
    SessionArtifactStoreError, SessionService, StateItemUpsert, budget_for_turn_intent,
    build_presigned_artifact_download, content_hash_with_normalize_version,
    expired_artifact_placeholder, runs::ToolOutputBatchItem,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row};
use uuid::Uuid;

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    astra_core::MatrixOneSettings::from_env()
}

static SHARED_POOL: tokio::sync::OnceCell<astra_core::SharedPool> =
    tokio::sync::OnceCell::const_new();

async fn setup_pool() -> astra_core::SharedPool {
    SHARED_POOL
        .get_or_init(|| async {
            let mut settings = require_db_it_env();
            settings.db_pool_max_connections = settings.db_pool_max_connections.clamp(1, 8);
            settings.db_pool_min_connections = settings
                .db_pool_min_connections
                .min(settings.db_pool_max_connections);
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".into());
            astra_services::ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema; is MatrixOne up?");
            astra_core::SharedPool::new(&settings)
                .await
                .expect("SharedPool::new")
        })
        .await
        .clone()
}

fn ids() -> (String, String, String) {
    let suffix = Uuid::new_v4();
    (
        format!("phase6-user-{suffix}"),
        format!("phase6-session-{suffix}"),
        format!("phase6-run-{suffix}"),
    )
}

async fn insert_session(pool: &astra_core::SharedPool, user_id: &str, session_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'phase6-agent', 'phase6 session', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .unwrap();
}

struct ArtifactSeed<'a> {
    user_id: &'a str,
    session_id: &'a str,
    artifact_id: &'a str,
    kind: &'a str,
    policy: &'a str,
    status: &'a str,
    retention_days: i64,
    manifest_refs: i64,
}

async fn insert_artifact(pool: &astra_core::SharedPool, seed: ArtifactSeed<'_>) {
    let retention_until =
        (chrono::Utc::now() + chrono::Duration::days(seed.retention_days)).naive_utc();
    sqlx::query(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, artifact_kind, content_json, metadata,
          retention_policy, retention_until, status, referenced_by_manifest_count,
         created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
    )
    .bind(seed.artifact_id)
    .bind(seed.session_id)
    .bind(seed.user_id)
    .bind(seed.kind)
    .bind(json!({"summary": seed.kind}).to_string())
    .bind(json!({"byte_size": 3_221_225_472_u64}).to_string())
    .bind(seed.policy)
    .bind(retention_until)
    .bind(seed.status)
    .bind(seed.manifest_refs)
    .execute(pool.get())
    .await
    .unwrap();
}

async fn artifact_status(
    pool: &astra_core::SharedPool,
    user_id: &str,
    session_id: &str,
    artifact_id: &str,
) -> (String, Option<String>) {
    let row = sqlx::query(
        "SELECT status, cold_storage_ref FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(artifact_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    (
        row.try_get("status").unwrap(),
        row.try_get::<Option<String>, _>("cold_storage_ref")
            .unwrap_or(None),
    )
}

async fn artifact_retention_until(
    pool: &astra_core::SharedPool,
    user_id: &str,
    session_id: &str,
    artifact_id: &str,
) -> Option<String> {
    sqlx::query_scalar(
        "SELECT CAST(retention_until AS CHAR) FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(artifact_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
}

fn content_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_49_byte_artifact_round_trip_reservation_seal_load_and_gc() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("workspace-package-{}", Uuid::new_v4());
    let first = b"tracked file\n".to_vec();
    let second = b"same workspace, new edge\n".to_vec();
    let first_digest = content_digest(&first);
    let second_digest = content_digest(&second);
    let mut aggregate = Sha256::new();
    aggregate.update(&first);
    aggregate.update(&second);
    let descriptor = SessionArtifactContentDescriptorV1 {
        schema_version: astra_services::SESSION_ARTIFACT_CONTENT_SCHEMA_VERSION,
        backend: astra_services::SESSION_ARTIFACT_CONTENT_BACKEND_MATRIXONE_CHUNKS_V1.to_string(),
        digest: format!("sha256:{:x}", aggregate.finalize()),
        byte_size: (first.len() + second.len()) as u64,
        chunk_count: 2,
        sealed: false,
    };
    let store = DatabaseSessionArtifactStore::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone());
    let record = SessionArtifactJsonRecord {
        artifact_id: artifact_id.clone(),
        session_id: session_id.clone(),
        user_id: user_id.clone(),
        artifact_kind: "workspace_snapshot_package".to_string(),
        source: Some("phase6".to_string()),
        turn: None,
        round: None,
        content: json!({"manifest_version": 1}),
        metadata: Some(json!({
            "workspace": {
                "files": ["src/lib.rs", "Cargo.toml"],
                "clean": false,
            },
            "capture": {"reason": "edge handoff"},
        })),
        references: Vec::new(),
    };
    store
        .begin_byte_artifact(record.clone(), descriptor.clone())
        .await
        .unwrap();
    store
        .put_content_chunk(&user_id, &session_id, &artifact_id, &first_digest, first.clone())
        .await
        .unwrap();
    store
        .put_content_chunk(
            &user_id,
            &session_id,
            &artifact_id,
            &second_digest,
            second.clone(),
        )
        .await
        .unwrap();

    // A stale, unsealed chunk remains reachable through the upload lease.
    sqlx::query(
        "UPDATE session_artifact_content_chunks
         SET created_at = DATE_SUB(NOW(6), INTERVAL 2 DAY),
             updated_at = DATE_SUB(NOW(6), INTERVAL 2 DAY)
         WHERE user_id = ? AND content_digest IN (?, ?)",
    )
    .bind(&user_id)
    .bind(&first_digest)
    .bind(&second_digest)
    .execute(pool.get())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE session_artifact_content_reservations
         SET expires_at = DATE_SUB(NOW(6), INTERVAL 2 DAY)
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?
           AND content_digest = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .bind(&first_digest)
    .execute(pool.get())
    .await
    .unwrap();
    // Uploading a later chunk renews the artifact-level lease, including the
    // first chunk that was not sent again by the client.
    store
        .put_content_chunk(
            &user_id,
            &session_id,
            &artifact_id,
            &second_digest,
            second.clone(),
        )
        .await
        .unwrap();
    let protected = run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    assert_eq!(protected.content_chunks_deleted, 0);

    // Begin is the explicit restart boundary. Both the upload lease and the
    // catalog deadline may be stale after a disconnected client; a successful
    // restart renews them atomically before the next sweeper pass.
    sqlx::query(
        "UPDATE session_artifacts
         SET retention_until = DATE_SUB(NOW(6), INTERVAL 2 DAY), status = 'active'
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .execute(pool.get())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE session_artifact_content_upload_leases
         SET expires_at = DATE_SUB(NOW(6), INTERVAL 2 DAY)
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .execute(pool.get())
    .await
    .unwrap();
    store
        .begin_byte_artifact(record.clone(), descriptor.clone())
        .await
        .unwrap();
    assert_eq!(artifact_status(&pool, &user_id, &session_id, &artifact_id).await.0, "active");
    let retention_live: i64 = sqlx::query_scalar(
        "SELECT CAST(retention_until > NOW(6) AS SIGNED) FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(retention_live, 1);
    let restarted_gc = run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    assert_eq!(restarted_gc.expired, 0);

    // Once the artifact lease itself expires, an upload racing the sweeper
    // must either lose the lease and fail cleanly or be observed before the
    // sweeper. In either case the shared row lock makes the interleaving
    // bounded; this branch exercises the expired-GC winner and restart path.
    sqlx::query(
        "UPDATE session_artifact_content_upload_leases
         SET expires_at = DATE_SUB(NOW(6), INTERVAL 2 DAY)
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .execute(pool.get())
    .await
    .unwrap();
    let (raced_put, raced_gc) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async {
            tokio::join!(
                store.put_content_chunk(
                    &user_id,
                    &session_id,
                    &artifact_id,
                    &second_digest,
                    second.clone(),
                ),
                run_artifact_retention_gc_once(pool.clone(), 100),
            )
        },
    )
    .await
    .expect("expired upload and GC must not deadlock");
    assert!(matches!(
        raced_put,
        Err(SessionArtifactStoreError::ContentUploadReservationExpired { .. })
    ));
    assert_eq!(raced_gc.unwrap().content_chunks_deleted, 2);

    // GC won the expired lease, so begin is the explicit restart boundary.
    store
        .begin_byte_artifact(record.clone(), descriptor.clone())
        .await
        .unwrap();
    store
        .put_content_chunk(&user_id, &session_id, &artifact_id, &first_digest, first.clone())
        .await
        .unwrap();
    store
        .put_content_chunk(
            &user_id,
            &session_id,
            &artifact_id,
            &second_digest,
            second.clone(),
        )
        .await
        .unwrap();

    let sealed = store
        .seal_byte_artifact(
            &user_id,
            &session_id,
            &artifact_id,
            vec![
                SessionArtifactContentChunkV1 {
                    chunk_index: 0,
                    digest: first_digest.clone(),
                    byte_size: first.len() as u64,
                },
                SessionArtifactContentChunkV1 {
                    chunk_index: 1,
                    digest: second_digest.clone(),
                    byte_size: second.len() as u64,
                },
            ],
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(sealed.status.as_deref(), Some("active"));
    let upload_lease_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifact_content_upload_leases
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(upload_lease_count, 0, "sealed artifact must not retain upload lease");
    // Replaying the original begin after a lost seal response is idempotent;
    // the server-owned sealed bit is intentionally excluded from the plan.
    store
        .begin_byte_artifact(record, descriptor.clone())
        .await
        .unwrap();
    let loaded = store
        .load_byte_artifact(&user_id, &session_id, &artifact_id)
        .await
        .unwrap()
        .unwrap();
    let mut expected_descriptor = descriptor.clone();
    expected_descriptor.sealed = true;
    assert_eq!(loaded.descriptor, expected_descriptor);
    assert_eq!(loaded.chunks[0].bytes, first);
    assert_eq!(loaded.chunks[1].bytes, second);

    sqlx::query(
        "UPDATE session_artifact_content_chunks
         SET created_at = DATE_SUB(NOW(6), INTERVAL 2 DAY),
             updated_at = DATE_SUB(NOW(6), INTERVAL 2 DAY)
         WHERE user_id = ? AND content_digest IN (?, ?)",
    )
    .bind(&user_id)
    .bind(&first_digest)
    .bind(&second_digest)
    .execute(pool.get())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE session_artifacts
         SET retention_until = DATE_SUB(NOW(6), INTERVAL 1 DAY), status = 'active'
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .execute(pool.get())
    .await
    .unwrap();
    let expired = run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    assert_eq!(expired.expired, 1);
    assert_eq!(expired.content_chunks_deleted, 2);
    assert_eq!(artifact_status(&pool, &user_id, &session_id, &artifact_id).await.0, "expired");
    assert!(store
        .load_byte_artifact(&user_id, &session_id, &artifact_id)
        .await
        .is_err());
}
}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_49b_shared_chunk_seals_use_global_lock_order() {
    let pool = setup_pool().await;
    let (user_id, session_a, _) = ids();
    let session_b = format!("phase6-session-b-{}", Uuid::new_v4());
    insert_session(&pool, &user_id, &session_a).await;
    insert_session(&pool, &user_id, &session_b).await;

    let first = b"shared header\n".to_vec();
    let second = b"shared body\n".to_vec();
    let first_digest = content_digest(&first);
    let second_digest = content_digest(&second);
    let descriptor = |ordered: &[&[u8]]| {
        let mut aggregate = Sha256::new();
        let mut byte_size = 0_u64;
        for bytes in ordered {
            aggregate.update(bytes);
            byte_size += bytes.len() as u64;
        }
        SessionArtifactContentDescriptorV1 {
            schema_version: astra_services::SESSION_ARTIFACT_CONTENT_SCHEMA_VERSION,
            backend: astra_services::SESSION_ARTIFACT_CONTENT_BACKEND_MATRIXONE_CHUNKS_V1
                .to_string(),
            digest: format!("sha256:{:x}", aggregate.finalize()),
            byte_size,
            chunk_count: ordered.len() as u64,
            sealed: false,
        }
    };
    let artifact_a = format!("workspace-a-{}", Uuid::new_v4());
    let artifact_b = format!("workspace-b-{}", Uuid::new_v4());
    let store = DatabaseSessionArtifactStore::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone());

    let record_a = SessionArtifactJsonRecord {
        artifact_id: artifact_a.clone(),
        session_id: session_a.clone(),
        user_id: user_id.clone(),
        artifact_kind: "workspace_snapshot_package".to_string(),
        source: Some("phase6-lock-order-a".to_string()),
        turn: None,
        round: None,
        content: json!({"order": "first-second"}),
        metadata: None,
        references: Vec::new(),
    };
    let record_b = SessionArtifactJsonRecord {
        artifact_id: artifact_b.clone(),
        session_id: session_b.clone(),
        user_id: user_id.clone(),
        artifact_kind: "workspace_snapshot_package".to_string(),
        source: Some("phase6-lock-order-b".to_string()),
        turn: None,
        round: None,
        content: json!({"order": "second-first"}),
        metadata: None,
        references: Vec::new(),
    };
    store
        .begin_byte_artifact(record_a, descriptor(&[&first, &second]))
        .await
        .unwrap();
    store
        .begin_byte_artifact(record_b, descriptor(&[&second, &first]))
        .await
        .unwrap();
    store
        .put_content_chunk(&user_id, &session_a, &artifact_a, &first_digest, first.clone())
        .await
        .unwrap();
    store
        .put_content_chunk(
            &user_id,
            &session_a,
            &artifact_a,
            &second_digest,
            second.clone(),
        )
        .await
        .unwrap();
    store
        .put_content_chunk(&user_id, &session_b, &artifact_b, &second_digest, second.clone())
        .await
        .unwrap();
    store
        .put_content_chunk(
            &user_id,
            &session_b,
            &artifact_b,
            &first_digest,
            first.clone(),
        )
        .await
        .unwrap();

    let (sealed_a, sealed_b) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async {
            tokio::join!(
                store.seal_byte_artifact(
                    &user_id,
                    &session_a,
                    &artifact_a,
                    vec![
                        SessionArtifactContentChunkV1 {
                            chunk_index: 0,
                            digest: first_digest.clone(),
                            byte_size: first.len() as u64,
                        },
                        SessionArtifactContentChunkV1 {
                            chunk_index: 1,
                            digest: second_digest.clone(),
                            byte_size: second.len() as u64,
                        },
                    ],
                    Vec::new(),
                ),
                store.seal_byte_artifact(
                    &user_id,
                    &session_b,
                    &artifact_b,
                    vec![
                        SessionArtifactContentChunkV1 {
                            chunk_index: 0,
                            digest: second_digest.clone(),
                            byte_size: second.len() as u64,
                        },
                        SessionArtifactContentChunkV1 {
                            chunk_index: 1,
                            digest: first_digest.clone(),
                            byte_size: first.len() as u64,
                        },
                    ],
                    Vec::new(),
                ),
            )
        },
    )
    .await
    .expect("shared chunk seals must not deadlock");
    assert!(sealed_a.is_ok(), "first seal failed: {sealed_a:?}");
    assert!(sealed_b.is_ok(), "second seal failed: {sealed_b:?}");
}
}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_49c_sealed_expiry_and_session_delete_do_not_deadlock() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("sealed-race-{}", Uuid::new_v4());
    let bytes = b"sealed race payload\n".to_vec();
    let digest = content_digest(&bytes);
    let mut aggregate = Sha256::new();
    aggregate.update(&bytes);
    let descriptor = SessionArtifactContentDescriptorV1 {
        schema_version: astra_services::SESSION_ARTIFACT_CONTENT_SCHEMA_VERSION,
        backend: astra_services::SESSION_ARTIFACT_CONTENT_BACKEND_MATRIXONE_CHUNKS_V1.to_string(),
        digest: format!("sha256:{:x}", aggregate.finalize()),
        byte_size: bytes.len() as u64,
        chunk_count: 1,
        sealed: false,
    };
    let store = DatabaseSessionArtifactStore::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone());
    store
        .begin_byte_artifact(
            SessionArtifactJsonRecord {
                artifact_id: artifact_id.clone(),
                session_id: session_id.clone(),
                user_id: user_id.clone(),
                artifact_kind: "workspace_snapshot_package".to_string(),
                source: Some("phase6-expiry-race".to_string()),
                turn: None,
                round: None,
                content: json!({"manifest_version": 1}),
                metadata: None,
                references: Vec::new(),
            },
            descriptor,
        )
        .await
        .unwrap();
    store
        .put_content_chunk(&user_id, &session_id, &artifact_id, &digest, bytes.clone())
        .await
        .unwrap();
    store
        .seal_byte_artifact(
            &user_id,
            &session_id,
            &artifact_id,
            vec![SessionArtifactContentChunkV1 {
                chunk_index: 0,
                digest: digest.clone(),
                byte_size: bytes.len() as u64,
            }],
            Vec::new(),
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE session_artifacts
         SET retention_until = DATE_SUB(NOW(6), INTERVAL 2 DAY), status = 'active'
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .execute(pool.get())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE session_artifact_content_chunks
         SET created_at = DATE_SUB(NOW(6), INTERVAL 2 DAY),
             updated_at = DATE_SUB(NOW(6), INTERVAL 2 DAY)
         WHERE user_id = ? AND content_digest = ?",
    )
    .bind(&user_id)
    .bind(&digest)
    .execute(pool.get())
    .await
    .unwrap();

    let service = DatabaseSessionService::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone());
    let (deleted, swept) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async {
            tokio::join!(
                service.delete_session(session_id.clone(), user_id.clone()),
                run_artifact_retention_gc_once(pool.clone(), 100),
            )
        },
    )
    .await
    .expect("sealed expiry and Session delete must not deadlock");
    deleted.expect("Session delete should succeed without saved Work");
    swept.expect("retention sweep should succeed");

    let session_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    let artifact_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(session_count, 0);
    assert_eq!(artifact_count, 0);
}

}

shared_db_test! {

#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_49d_state_projection_and_session_delete_are_fenced() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("state-race-artifact-{}", Uuid::new_v4());
    insert_artifact(
        &pool,
        ArtifactSeed {
            user_id: &user_id,
            session_id: &session_id,
            artifact_id: &artifact_id,
            kind: "state_projection_fixture",
            policy: "default",
            status: "active",
            retention_days: 1,
            manifest_refs: 0,
        },
    )
    .await;

    let projection = DatabaseStateProjectionStore::new(pool.clone());
    let item = StateItemUpsert {
        item_id: Some(format!("state-race-{}", Uuid::new_v4())),
        user_id: user_id.clone(),
        session_id: session_id.clone(),
        scope: "session".to_string(),
        category: "finding".to_string(),
        item_key: "artifact-race".to_string(),
        status: "active".to_string(),
        priority: 10,
        source: "phase6-race".to_string(),
        provenance_event_id: None,
        run_id: None,
        title: Some("state projection race".to_string()),
        summary_text: Some("the writer and delete must settle cleanly".to_string()),
        payload_json: json!({"artifact_id": artifact_id}),
        token_estimate: 32,
        mutation: "insert".to_string(),
    };
    let service = DatabaseSessionService::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone());
    let (write, deleted) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async {
            tokio::join!(
                projection.upsert_state_item(item),
                service.delete_session(session_id.clone(), user_id.clone()),
            )
        },
    )
    .await
    .expect("state projection and Session delete must not deadlock");

    deleted.expect("Session delete should succeed without saved Work");
    if let Err(error) = write {
        assert!(
            matches!(error, astra_services::StateProjectionError::SessionNotActive { .. }),
            "a losing state writer should fail with an actionable lifecycle error: {error}"
        );
    }

    let row = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?) AS sessions,
            (SELECT COUNT(*) FROM session_state_items WHERE user_id = ? AND session_id = ?) AS state_items,
            (SELECT COUNT(*) FROM session_state_item_events WHERE user_id = ? AND session_id = ?) AS state_events,
            (SELECT COUNT(*) FROM session_artifacts WHERE user_id = ? AND session_id = ?) AS artifacts",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("sessions").unwrap(), 0);
    assert_eq!(row.try_get::<i64, _>("state_items").unwrap(), 0);
    assert_eq!(row.try_get::<i64, _>("state_events").unwrap(), 0);
    assert_eq!(row.try_get::<i64, _>("artifacts").unwrap(), 0);
}
}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_50_gc_preserves_referenced_artifact_deadline_without_fake_cold_storage() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("artifact-{}", Uuid::new_v4());
    insert_artifact(
        &pool,
        ArtifactSeed {
            user_id: &user_id,
            session_id: &session_id,
            artifact_id: &artifact_id,
            kind: "pg_dump",
            policy: "default",
            status: "active",
            retention_days: 1,
            manifest_refs: 1,
        },
    )
    .await;

    let deadline_before =
        artifact_retention_until(&pool, &user_id, &session_id, &artifact_id).await;
    let _outcome = run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    let (status, cold_ref) = artifact_status(&pool, &user_id, &session_id, &artifact_id).await;
    let deadline_after = artifact_retention_until(&pool, &user_id, &session_id, &artifact_id).await;
    assert_eq!(deadline_after, deadline_before);
    assert_eq!(status, "active");
    assert_eq!(
        cold_ref, None,
        "no cold reference exists until bytes are moved"
    );
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_50b_gc_honors_durable_reference_edges_without_payload_inference() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("artifact-{}", Uuid::new_v4());
    insert_artifact(
        &pool,
        ArtifactSeed {
            user_id: &user_id,
            session_id: &session_id,
            artifact_id: &artifact_id,
            kind: "arbitrary_provider_result",
            policy: "default",
            status: "active",
            retention_days: 1,
            manifest_refs: 0,
        },
    )
    .await;
    sqlx::query(
        "INSERT INTO session_artifact_references
         (user_id, session_id, artifact_id, reference_kind, reference_id, created_at)
         VALUES (?, ?, ?, 'invocation_ledger', ?, NOW(6))",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .bind(format!("invocation:{}", Uuid::new_v4()))
    .execute(pool.get())
    .await
    .unwrap();

    let deadline_before =
        artifact_retention_until(&pool, &user_id, &session_id, &artifact_id).await;
    let _outcome = run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    let (status, cold_ref) = artifact_status(&pool, &user_id, &session_id, &artifact_id).await;
    let deadline_after = artifact_retention_until(&pool, &user_id, &session_id, &artifact_id).await;
    assert_eq!(deadline_after, deadline_before);
    assert_eq!(status, "active");
    assert_eq!(cold_ref, None);
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_51_unknown_tool_uses_400b_fallback_and_writes_warning_event() {
    let pool = setup_pool().await;
    let (user_id, session_id, run_id) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseRunStateStore::new(pool.clone());
    let tool_name = format!("unknown_phase6_{}", Uuid::new_v4().simple());
    store
        .insert_tool_output_batch(
            &format!("batch-{}", Uuid::new_v4()),
            &session_id,
            &run_id,
            &user_id,
            &[ToolOutputBatchItem {
                output_id: format!("out-{}", Uuid::new_v4()),
                tool_call_id: Some("call-1".to_string()),
                tool_name: tool_name.clone(),
                result: astra_turn_types::ToolInvocationResultPayload::new(
                    "x".repeat(1_200),
                    Default::default(),
                    None,
                )
                .unwrap(),
            }],
        )
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT
           (SELECT preview_status FROM session_tool_outputs WHERE user_id = ? AND session_id = ? AND tool_name = ? LIMIT 1) AS preview_status,
           (SELECT CHAR_LENGTH(preview_text) FROM session_tool_outputs WHERE user_id = ? AND session_id = ? AND tool_name = ? LIMIT 1) AS preview_len,
           (SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'preview_template_missing' AND meta_tool_name = ?) AS warning_count",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&tool_name)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&tool_name)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&tool_name)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        row.try_get::<String, _>("preview_status").unwrap(),
        "fallback"
    );
    assert!(row.try_get::<u64, _>("preview_len").unwrap() <= 400);
    assert_eq!(row.try_get::<i64, _>("warning_count").unwrap(), 1);
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_52_preview_template_normalize_versions_are_seeded_and_deterministic() {
    let pool = setup_pool().await;
    let rows = sqlx::query(
        "SELECT tool_name, normalize_version FROM preview_template_registry
         WHERE tool_name IN ('pg_dump', 'fetch_url', 'parse_pdf', 'SKILL.md',
             'list_dir', 'task_board', 'agent_fanout', 'session', 'web_fetch', 'mo_query')
           AND status = 'active'",
    )
    .fetch_all(pool.get())
    .await
    .unwrap();
    let seeded = rows
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>("tool_name").unwrap(),
                row.try_get::<String, _>("normalize_version").unwrap(),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    for expected in [
        "pg_dump",
        "fetch_url",
        "parse_pdf",
        "SKILL.md",
        "list_dir",
        "task_board",
        "agent_fanout",
        "session",
        "web_fetch",
        "mo_query",
    ] {
        let normalize_version = seeded.get(expected).expect("baseline template missing");
        let a = content_hash_with_normalize_version("sha256:content", Some(normalize_version));
        let b = content_hash_with_normalize_version("sha256:content", Some(normalize_version));
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"));
    }
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_53_large_pg_dump_uses_artifact_ref_and_never_prompt_raw_payload() {
    let pool = setup_pool().await;
    let (user_id, session_id, run_id) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseRunStateStore::new(pool.clone());
    let output_id = format!("out-{}", Uuid::new_v4());
    let raw = "CREATE TABLE t(a INT);\n".repeat(2_000);
    store
        .insert_tool_output_batch(
            &format!("batch-{}", Uuid::new_v4()),
            &session_id,
            &run_id,
            &user_id,
            &[ToolOutputBatchItem {
                output_id: output_id.clone(),
                tool_call_id: Some("call-pg".to_string()),
                tool_name: "pg_dump".to_string(),
                result: astra_turn_types::ToolInvocationResultPayload::bounded_projection(
                    raw,
                    std::collections::BTreeMap::from([(
                        "declared_size_bytes".to_string(),
                        json!(3_221_225_472_u64),
                    )]),
                    None,
                ),
            }],
        )
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT artifact_ref, CHAR_LENGTH(preview_text) AS preview_len, payload_bytes
         FROM session_tool_outputs
         WHERE user_id = ? AND session_id = ? AND output_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&output_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert!(
        row.try_get::<Option<String>, _>("artifact_ref")
            .unwrap()
            .as_deref()
            .is_some_and(|value| value.starts_with("tool_output://"))
    );
    assert!(row.try_get::<u64, _>("preview_len").unwrap() <= 1000);
    assert!(row.try_get::<i64, _>("payload_bytes").unwrap() > 1000);
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_54_project_long_term_artifact_is_extended_not_expired() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("artifact-{}", Uuid::new_v4());
    insert_artifact(
        &pool,
        ArtifactSeed {
            user_id: &user_id,
            session_id: &session_id,
            artifact_id: &artifact_id,
            kind: "benchmark",
            policy: "project_long_term",
            status: "active",
            retention_days: -1,
            manifest_refs: 0,
        },
    )
    .await;
    run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    let (status, _) = artifact_status(&pool, &user_id, &session_id, &artifact_id).await;
    assert_eq!(status, "active");
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_54b_expiry_removes_retained_payload_bytes_instead_of_only_relabeling() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("artifact-{}", Uuid::new_v4());
    insert_artifact(
        &pool,
        ArtifactSeed {
            user_id: &user_id,
            session_id: &session_id,
            artifact_id: &artifact_id,
            kind: "raw_provider_result",
            policy: "default",
            status: "active",
            retention_days: -1,
            manifest_refs: 0,
        },
    )
    .await;
    run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT status, content_json, CAST(metadata AS CHAR) AS metadata_json
         FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "expired");
    let content: serde_json::Value =
        serde_json::from_str(&row.try_get::<String, _>("content_json").unwrap()).unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(&row.try_get::<String, _>("metadata_json").unwrap()).unwrap();
    assert_eq!(
        content,
        json!({"expired": true, "reason": "retention_elapsed"})
    );
    assert_eq!(metadata, json!({"retentionExpired": true}));
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_55_presigned_download_contains_ttl_and_signature() {
    let signed = build_presigned_artifact_download(
        "/sessions/s/artifacts/a/download/presigned",
        "user-1",
        "session-1",
        "artifact-1",
        "secret",
        chrono::Utc::now(),
        300,
    );
    assert_eq!(signed.method, "GET");
    assert!(signed.download_url.contains("expires_at="));
    assert!(
        signed.download_url.contains("signature=sha256%3A")
            || signed.download_url.contains("signature=sha256:")
    );
    assert!(signed.signature.starts_with("sha256:"));
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_56_expired_artifact_renders_historical_placeholder() {
    let placeholder = expired_artifact_placeholder("artifact-x", Some("row count preserved"));
    assert!(placeholder.contains("historical, raw no longer available"));
    assert!(placeholder.contains("summary preserved"));
    assert!(placeholder.contains("row count preserved"));
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_57_benchmark_comparison_expands_tool_previews_from_recent_tail() {
    let normal = budget_for_turn_intent(Some("normal"));
    let benchmark = budget_for_turn_intent(Some("benchmark_comparison"));
    assert!(!normal.flex_applied);
    assert!(benchmark.flex_applied);
    assert_eq!(benchmark.budget.tool_previews, 2_500);
    assert_eq!(benchmark.budget.recent_tail, 1_600);
    assert_eq!(
        benchmark.borrowed_from_recent_tail,
        normal.budget.recent_tail - benchmark.budget.recent_tail
    );
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_17_s08_dba_audit_large_artifacts_batch_and_gc() {
    let pool = setup_pool().await;
    let (user_id, session_id, run_id) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_ids = ["pg-dump", "tool-batch", "slowlog"]
        .into_iter()
        .map(|kind| format!("{kind}-{}", Uuid::new_v4()))
        .collect::<Vec<_>>();
    for (idx, artifact_id) in artifact_ids.iter().enumerate() {
        insert_artifact(
            &pool,
            ArtifactSeed {
                user_id: &user_id,
                session_id: &session_id,
                artifact_id,
                kind: if idx == 2 { "slowlog" } else { "pg_dump" },
                policy: "default",
                status: "active",
                retention_days: 1,
                manifest_refs: if idx == 0 { 1 } else { 0 },
            },
        )
        .await;
    }

    let store = DatabaseRunStateStore::new(pool.clone());
    let mut items = Vec::with_capacity(500);
    for i in 0..1_000 {
        items.push(ToolOutputBatchItem {
            output_id: format!("out-{i}-{}", Uuid::new_v4()),
            tool_call_id: Some(format!("call-{i}")),
            tool_name: "slow_query_analyzer".to_string(),
            result: astra_turn_types::ToolInvocationResultPayload::new(
                format!("slow query {i}"),
                std::collections::BTreeMap::from([(
                    "declared_size_bytes".to_string(),
                    json!(838_860_800_u64),
                )]),
                None,
            )
            .unwrap(),
        });
        if items.len() == 500 {
            store
                .insert_tool_output_batch(
                    &format!("batch-{i}-{}", Uuid::new_v4()),
                    &session_id,
                    &run_id,
                    &user_id,
                    &items,
                )
                .await
                .unwrap();
            items.clear();
        }
    }
    let row = sqlx::query(
        "SELECT
          (SELECT COUNT(*) FROM session_tool_outputs FORCE INDEX (idx_tool_outputs_user_run_created) WHERE user_id = ? AND run_id = ?) AS output_count,
          (SELECT COUNT(*) FROM session_artifacts FORCE INDEX (idx_session_artifacts_owner_kind_order) WHERE user_id = ? AND session_id = ? AND artifact_kind = 'pg_dump') AS dump_count,
          (SELECT COUNT(*) FROM session_artifacts FORCE INDEX (idx_artifacts_retention) WHERE retention_until IS NOT NULL AND retention_until <= DATE_ADD(NOW(6), INTERVAL 7 DAY) AND (status <=> 'active' OR status <=> 'expiring')) AS retention_count",
    )
    .bind(&user_id)
    .bind(&run_id)
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("output_count").unwrap(), 1_000);
    assert!(row.try_get::<i64, _>("dump_count").unwrap() >= 2);
    assert!(row.try_get::<i64, _>("retention_count").unwrap() >= 1);

    let deadline_before =
        artifact_retention_until(&pool, &user_id, &session_id, &artifact_ids[0]).await;
    let _retention = run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    let (_, cold_ref) = artifact_status(&pool, &user_id, &session_id, &artifact_ids[0]).await;
    let deadline_after =
        artifact_retention_until(&pool, &user_id, &session_id, &artifact_ids[0]).await;
    assert_eq!(deadline_after, deadline_before);
    assert_eq!(cold_ref, None, "sweeper must not fabricate cold storage");
}

}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_18_s12_14_day_review_retention_and_benchmark_budget_flex() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let seeded_at = chrono::Utc::now();
    let mut insert = QueryBuilder::<MySql>::new(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, artifact_kind, content_json, metadata,
          retention_policy, retention_until, status, referenced_by_manifest_count,
          created_at, updated_at) ",
    );
    insert.push_values(0..250, |mut row, i| {
        let kind = if i % 2 == 0 { "fetch_url" } else { "parse_pdf" };
        let policy = if i % 25 == 0 {
            "project_long_term"
        } else {
            "default"
        };
        let retention_until = (seeded_at + chrono::Duration::days((i % 14) as i64 - 7)).naive_utc();
        row.push_bind(format!("review-artifact-{i}-{}", Uuid::new_v4()))
            .push_bind(&session_id)
            .push_bind(&user_id)
            .push_bind(kind)
            .push_bind(json!({"summary": kind}).to_string())
            .push_bind(json!({"byte_size": 3_221_225_472_u64}).to_string())
            .push_bind(policy)
            .push_bind(retention_until)
            .push_bind("active")
            .push_bind(0_i64)
            .push("NOW(6)")
            .push("NOW(6)");
    });
    insert.build().execute(pool.get()).await.unwrap();

    run_artifact_retention_gc_once(pool.clone(), 500)
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT
           COUNT(*) AS total_artifacts,
           CAST(COALESCE(SUM(CASE
                 WHEN retention_policy = 'project_long_term'
                  AND status = 'active'
                  AND retention_until > DATE_ADD(NOW(6), INTERVAL 300 DAY)
                 THEN 1 ELSE 0
               END), 0) AS SIGNED) AS long_term_extended,
           CAST(COALESCE(SUM(CASE
                 WHEN retention_policy = 'default'
                  AND (status = 'expired' OR status = 'expiring')
                 THEN 1 ELSE 0
               END), 0) AS SIGNED) AS default_processed,
           CAST(COALESCE(SUM(CASE
                 WHEN retention_policy = 'default' AND status = 'active'
                 THEN 1 ELSE 0
               END), 0) AS SIGNED) AS default_still_active
         FROM session_artifacts WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("total_artifacts").unwrap(), 250);
    assert_eq!(row.try_get::<i64, _>("long_term_extended").unwrap(), 10);
    assert_eq!(row.try_get::<i64, _>("default_processed").unwrap(), 240);
    assert_eq!(row.try_get::<i64, _>("default_still_active").unwrap(), 0);
    let budget = budget_for_turn_intent(Some("benchmark_comparison"));
    assert_eq!(budget.budget.tool_previews, 2_500);
    assert!(budget.borrowed_from_recent_tail > 0);
}
}
