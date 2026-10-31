mod test_support;

use test_support::require_db_it_env;

use astra_runtime::server::artifact_retention_sweeper::run_artifact_retention_gc_once;
use astra_services::{
    DatabaseSessionArtifactStore, DatabaseSessionService, SessionArtifactContentChunkV1,
    SessionArtifactContentDescriptorV1, SessionArtifactContentStore, SessionArtifactJsonRecord,
    SessionArtifactJsonStore, SessionArtifactReference, SessionArtifactReferenceKind,
    SessionArtifactStoreError, SessionService, build_presigned_artifact_download,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row};
use uuid::Uuid;

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
    citation_refs: i64,
}

async fn insert_artifact(pool: &astra_core::SharedPool, seed: ArtifactSeed<'_>) {
    let retention_until =
        (chrono::Utc::now() + chrono::Duration::days(seed.retention_days)).naive_utc();
    sqlx::query(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, artifact_kind, content_json, metadata,
          retention_policy, retention_until, status, referenced_by_citation_count,
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
    .bind(seed.citation_refs)
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
    assert_eq!(sealed.artifact.status.as_deref(), Some("active"));
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
async fn byte_artifact_batch_reads_preserve_duplicates_and_reject_corrupt_sizes() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("batch-content-{}", Uuid::new_v4());
    let mut payloads = (0..130).map(|index| format!("chunk-{index}").into_bytes()).collect::<Vec<_>>();
    payloads.push(payloads[0].clone());
    let all = payloads.concat();
    let mut descriptor = SessionArtifactContentDescriptorV1::new(
        astra_services::SESSION_ARTIFACT_CONTENT_BACKEND_MATRIXONE_CHUNKS_V1, content_digest(&all), all.len() as u64,
    );
    descriptor.chunk_count = payloads.len() as u64;
    let store = DatabaseSessionArtifactStore::new(pool.settings().clone()).with_pool(pool.clone());
    store.begin_byte_artifact(SessionArtifactJsonRecord {
        artifact_id: artifact_id.clone(), session_id: session_id.clone(), user_id: user_id.clone(),
        artifact_kind: "bytes".into(), source: None, turn: None, round: None,
        content: json!({"purpose":"batch-read-contract"}), metadata: None, references: Vec::new(),
    }, descriptor).await.unwrap();
    let mut refs = Vec::new();
    for (index, bytes) in payloads.iter().enumerate() {
        let digest = content_digest(bytes);
        store.put_content_chunk(&user_id, &session_id, &artifact_id, &digest, bytes.clone()).await.unwrap();
        refs.push(SessionArtifactContentChunkV1 { chunk_index: index as u64, digest, byte_size: bytes.len() as u64 });
    }
    let sealed = store.seal_byte_artifact(&user_id, &session_id, &artifact_id, refs.clone(), Vec::new()).await.unwrap();
    let replay = store.seal_byte_artifact(&user_id, &session_id, &artifact_id, refs.clone(), Vec::new()).await.unwrap();
    let loaded = store.load_byte_artifact(&user_id, &session_id, &artifact_id).await.unwrap().unwrap();
    assert_eq!(sealed.descriptor, replay.descriptor);
    assert_eq!(sealed.descriptor, loaded.descriptor);
    for bundle in [&sealed, &replay, &loaded] {
        assert_eq!(bundle.chunks.iter().map(|chunk| chunk.bytes.as_slice()).collect::<Vec<_>>(), payloads.iter().map(Vec::as_slice).collect::<Vec<_>>());
    }
    let digest = content_digest(&payloads[0]);
    let corrupt = vec![b'x'; 4096];
    sqlx::query("UPDATE session_artifact_content_chunks SET byte_size = ?, content = ? WHERE user_id = ? AND content_digest = ?")
        .bind(corrupt.len() as u64).bind(corrupt).bind(&user_id).bind(&digest).execute(pool.get()).await.unwrap();
    assert!(matches!(store.seal_byte_artifact(&user_id, &session_id, &artifact_id, refs, Vec::new()).await,
        Err(SessionArtifactStoreError::ContentChunkSizeMismatch { .. })));
    assert!(matches!(store.load_byte_artifact(&user_id, &session_id, &artifact_id).await,
        Err(SessionArtifactStoreError::ContentChunkSizeMismatch { .. })));
    assert!(matches!(store.load_byte_artifact_chunk(&user_id, &session_id, &artifact_id, &digest).await,
        Err(SessionArtifactStoreError::ContentChunkSizeMismatch { .. })));
    sqlx::query("UPDATE session_artifact_content_chunks SET byte_size = ?, content = ? WHERE user_id = ? AND content_digest = ?")
        .bind(payloads[0].len() as u64).bind(&payloads[0]).bind(&user_id).bind(&digest).execute(pool.get()).await.unwrap();
    assert!(store.load_byte_artifact(&user_id, &session_id, &artifact_id).await.unwrap().is_some());
    DatabaseSessionService::new(pool.settings().clone()).with_pool(pool)
        .delete_session(session_id, user_id).await.unwrap();
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
async fn l2_49d_artifact_reference_and_session_delete_are_fenced() {
    use std::time::Duration;
    async fn observe_fence_waiters(
        connection: &mut sqlx::MySqlConnection,
        actors: [(u64, &str); 2],
        user: &str,
        session: &str,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut previous = [None, None];
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err("actor never reached its database barrier".into());
            }
            let rows = tokio::time::timeout_at(
                deadline,
                sqlx::query("SHOW PROCESSLIST").fetch_all(&mut *connection),
            )
            .await
            .map_err(|_| "actor never reached its database barrier".to_string())?
            .map_err(|error| error.to_string())?;
            let mut current = [None, None];
            for row in rows {
                let id = row.try_get::<u64, _>("conn_id").map_err(|error| error.to_string())?;
                let Some(index) = actors.iter().position(|(actor, _)| *actor == id) else { continue; };
                let command: String =
                    row.try_get("command").map_err(|error| error.to_string())?;
                let info: String = row.try_get("info").map_err(|error| error.to_string())?;
                if matches!(command.as_str(), "COM_STMT_EXECUTE" | "COM_QUERY")
                    && [actors[index].1, "agent_session_lifecycle_fences", user, session]
                        .iter().all(|fragment| info.contains(fragment))
                {
                    let statement: String = row
                        .try_get("statement_id")
                        .map_err(|error| error.to_string())?;
                    if !statement.is_empty() {
                        current[index] = Some(statement);
                    }
                }
            }
            if current.iter().all(Option::is_some) && current == previous {
                return Ok(());
            }
            previous = current;
            tokio::task::yield_now().await;
        }
    }

    async fn finish<T>(mut task: tokio::task::JoinHandle<T>) -> Result<T, String> {
        match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err("database actor did not finish after releasing its barrier".into())
            }
        }
    }

    let pool = setup_pool().await;
    let mut settings = require_db_it_env();
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let writer_pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let deletion_pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let observer_pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let artifacts = DatabaseSessionArtifactStore::new(settings.clone()).with_pool(writer_pool.clone());
    let service = DatabaseSessionService::new(settings).with_pool(deletion_pool.clone());
    let mut observer = observer_pool.get().acquire().await.unwrap();
    for order in ["reference_first", "deletion_first", "contended"] {
        let (user_id, session_id, _) = ids();
        insert_session(&pool, &user_id, &session_id).await;
        let artifact_id = Uuid::new_v4().to_string();
        insert_artifact(
            &pool,
            ArtifactSeed {
                user_id: &user_id,
                session_id: &session_id,
                artifact_id: &artifact_id,
                kind: "reference_race_fixture",
                policy: "default",
                status: "active",
                retention_days: 1,
                citation_refs: 0,
            },
        )
        .await;
        let reference = SessionArtifactReference {
            kind: SessionArtifactReferenceKind::Citation,
            reference_id: Uuid::new_v4().to_string(),
        };
        if order == "reference_first" {
            assert!(
                artifacts
                    .retain_json_artifact_reference(&user_id, &session_id, &artifact_id, &reference)
                    .await
                    .unwrap()
            );
            assert_eq!(
                artifacts
                    .list_json_artifact_references(&user_id, &session_id, &artifact_id, 10)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            service
                .delete_session(session_id.clone(), user_id.clone())
                .await
                .unwrap();
        } else if order == "deletion_first" {
            service
                .delete_session(session_id.clone(), user_id.clone())
                .await
                .unwrap();
        } else {
            // Use the setup pool so the actor connections have no earlier
            // matching statement for this unique Session in process-list history.
            let seeded = DatabaseSessionArtifactStore::new(pool.settings().clone()).with_pool(pool.clone());
            assert!(
                seeded
                    .retain_json_artifact_reference(&user_id, &session_id, &artifact_id, &reference)
                    .await
                    .unwrap()
            );
            assert!(
                seeded
                    .release_json_artifact_reference(
                        &user_id,
                        &session_id,
                        &artifact_id,
                        &reference
                    )
                    .await
                    .unwrap()
            );
            // Query the physical identities on each exclusive actor pool.
            let writer_connection: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
                .fetch_one(writer_pool.get()).await.unwrap();
            let deletion_connection: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
                .fetch_one(deletion_pool.get()).await.unwrap();
            assert_ne!(writer_connection, deletion_connection);
            let mut blocker = pool.get().begin().await.unwrap();
            astra_services::storage::lock_agent_session_write_fence(
                &mut blocker, &session_id, &user_id,
            ).await.unwrap();
            let write = {
                let artifacts = artifacts.clone();
                let (user, session, artifact, reference) =
                    (user_id.clone(), session_id.clone(), artifact_id.clone(), reference.clone());
                tokio::spawn(async move {
                    artifacts.retain_json_artifact_reference(&user, &session, &artifact, &reference).await
                })
            };
            let deleted = {
                let service = service.clone();
                let (user, session) = (user_id.clone(), session_id.clone());
                tokio::spawn(async move { service.delete_session(session, user).await })
            };
            let observed = observe_fence_waiters(
                &mut observer,
                [(writer_connection, "SELECT delete_requested_at"),
                 (deletion_connection, "INSERT IGNORE INTO agent_session_lifecycle_fences")],
                &user_id, &session_id,
            ).await;
            let pending = !write.is_finished() && !deleted.is_finished();
            // Always release and join/abort both actors before reporting an
            // observation failure, so failed evidence cannot strand a worker.
            let released = blocker.commit().await;
            let write = finish(write).await;
            let deleted = finish(deleted).await;
            observed.expect("both actors reach the held database fence");
            assert!(pending);
            released.unwrap();
            deleted.unwrap().unwrap();
            let write = write.unwrap();
            assert!(matches!(write, Ok(true) | Err(SessionArtifactStoreError::SessionNotOwned { .. })));

        }
        assert!(matches!(
            artifacts
                .retain_json_artifact_reference(&user_id, &session_id, &artifact_id, &reference)
                .await,
            Err(SessionArtifactStoreError::SessionNotOwned { .. })
        ));
        for table in [
            "agent_sessions",
            "session_artifacts",
            "session_artifact_references",
        ] {
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND session_id = ?"
            )))
            .bind(&user_id)
            .bind(&session_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
            assert_eq!(count, 0, "{order}: no orphan {table}");
        }
    }
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
            retention_days: -1,
            citation_refs: 1,
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
    let store = DatabaseSessionArtifactStore::new(astra_core::MatrixOneSettings::from_env()).with_pool(pool.clone());
    assert!(sqlx::query("INSERT INTO session_artifact_references (user_id, session_id, artifact_id, reference_kind, reference_id) VALUES (?, ?, 'retired-kind', 'state_item', 'retired')")
        .bind(&user_id).bind(&session_id).execute(pool.get()).await.is_err());
    for kind in [
        SessionArtifactReferenceKind::InvocationLedger,
        SessionArtifactReferenceKind::Manifest,
        SessionArtifactReferenceKind::Citation,
        SessionArtifactReferenceKind::RecoveryPoint,
        SessionArtifactReferenceKind::SessionTranscript,
    ] {
        let artifact_id = format!("artifact-{}", Uuid::new_v4());
        insert_artifact(&pool, ArtifactSeed {
            user_id: &user_id,
            session_id: &session_id,
            artifact_id: &artifact_id,
            kind: "arbitrary_provider_result",
            policy: "default",
            status: "active",
            retention_days: -1,
            citation_refs: 0,
        }).await;
        let reference = SessionArtifactReference {
            kind,
            reference_id: if kind == SessionArtifactReferenceKind::SessionTranscript {
                session_id.clone()
            } else {
                format!("retention-{}", Uuid::new_v4())
            },
        };
        store.retain_json_artifact_reference(&user_id, &session_id, &artifact_id, &reference).await.unwrap();
        // Retaining extends the deadline. Move it into the past to exercise
        // expiry eligibility while the durable reference still exists.
        sqlx::query("UPDATE session_artifacts SET retention_until = TIMESTAMPADD(DAY, -1, NOW(6)) WHERE user_id = ? AND session_id = ? AND artifact_id = ?")
            .bind(&user_id).bind(&session_id).bind(&artifact_id).execute(pool.get()).await.unwrap();
        let deadline = artifact_retention_until(&pool, &user_id, &session_id, &artifact_id).await;
        run_artifact_retention_gc_once(pool.clone(), 100).await.unwrap();
        assert_eq!(artifact_status(&pool, &user_id, &session_id, &artifact_id).await, ("active".into(), None), "{kind:?}");
        assert_eq!(artifact_retention_until(&pool, &user_id, &session_id, &artifact_id).await, deadline);
        store.release_json_artifact_reference(&user_id, &session_id, &artifact_id, &reference).await.unwrap();
        run_artifact_retention_gc_once(pool.clone(), 100).await.unwrap();
        assert_eq!(artifact_status(&pool, &user_id, &session_id, &artifact_id).await, ("expired".into(), None), "{kind:?}");
        assert_eq!(artifact_retention_until(&pool, &user_id, &session_id, &artifact_id).await, deadline);
    }
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
            citation_refs: 0,
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
            citation_refs: 0,
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
async fn l3_18_s12_14_day_review_retention() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let seeded_at = chrono::Utc::now();
    let mut insert = QueryBuilder::<MySql>::new(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, artifact_kind, content_json, metadata,
          retention_policy, retention_until, status, referenced_by_citation_count,
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
}
}

shared_db_test! {
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn context_history_session_reference_survives_gc_and_owned_session_deletion_releases_it() {
    let pool = setup_pool().await;
    let (user_id, session_id, run_id) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseSessionArtifactStore::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone());
    let id = Uuid::new_v4().to_string();
    let reference = SessionArtifactReference {
        kind: SessionArtifactReferenceKind::SessionTranscript,
        reference_id: session_id.clone(),
    };
    let history = astra_turn_types::ContextHistoryArtifactV1::new(
        &session_id,
        &run_id,
        1,
        serde_json::to_string(&json!([{"role":"user","content":"exact historical request"}]))
            .unwrap(),
    )
    .unwrap();
    let record = SessionArtifactJsonRecord {
        artifact_id: id.clone(),
        session_id: session_id.clone(),
        user_id: user_id.clone(),
        artifact_kind: astra_turn_types::CONTEXT_HISTORY_ARTIFACT_KIND.into(),
        source: None,
        turn: None,
        round: None,
        content: serde_json::to_value(history).unwrap(),
        metadata: None,
        references: vec![reference.clone()],
    };
    store.persist_json_artifact(record.clone()).await.unwrap();
    assert_eq!(
        store
            .list_json_artifact_references(&user_id, &session_id, &id, 10)
            .await
            .unwrap(),
        vec![reference.clone()]
    );
    assert_eq!(
        store
            .list_json_artifacts_for_reference(&user_id, &session_id, &reference, 10)
            .await
            .unwrap(),
        vec![id.clone()]
    );
    sqlx::query(
        "UPDATE agent_sessions SET status = 'deleting' WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .unwrap();
    let mut late_record = record.clone();
    late_record.artifact_id = Uuid::new_v4().to_string();
    assert!(matches!(
        store.persist_json_artifact(late_record.clone()).await,
        Err(SessionArtifactStoreError::SessionNotOwned { .. })
    ));
    assert!(matches!(
        store
            .retain_json_artifact_reference(&user_id, &session_id, &id, &reference)
            .await,
        Err(SessionArtifactStoreError::SessionNotOwned { .. })
    ));
    sqlx::query("UPDATE agent_sessions SET status = 'active' WHERE user_id = ? AND session_id = ?")
        .bind(&user_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .unwrap();
    sqlx::query("UPDATE session_artifacts SET retention_until = DATE_SUB(NOW(6), INTERVAL 1 DAY) WHERE user_id = ? AND session_id = ? AND artifact_id = ?")
        .bind(&user_id).bind(&session_id).bind(&id).execute(pool.get()).await.unwrap();
    run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    assert_eq!(
        artifact_status(&pool, &user_id, &session_id, &id).await.0,
        "active"
    );
    let service = DatabaseSessionService::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone());
    let denial = service
        .delete_session(session_id.clone(), "foreign-owner".into())
        .await
        .unwrap_err();
    assert_eq!(denial.0, axum::http::StatusCode::NOT_FOUND);
    assert!(
        store
            .load_json_artifact(&user_id, &session_id, &id)
            .await
            .unwrap()
            .is_some()
    );
    service
        .delete_session(session_id.clone(), user_id.clone())
        .await
        .unwrap();
    let references: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifact_references WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    let artifacts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifacts WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!((references, artifacts), (0, 0));
    assert!(matches!(
        store.persist_json_artifact(late_record).await,
        Err(SessionArtifactStoreError::SessionNotOwned { .. })
    ));
    assert!(matches!(
        store
            .retain_json_artifact_reference(&user_id, &session_id, &id, &reference)
            .await,
        Err(SessionArtifactStoreError::SessionNotOwned { .. })
    ));
}
}
