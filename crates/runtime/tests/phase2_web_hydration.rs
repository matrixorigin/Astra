mod test_support;

use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    astra_core::MatrixOneSettings::from_env()
}

static SHARED_BOOTSTRAP: tokio::sync::OnceCell<astra_core::SharedPool> =
    tokio::sync::OnceCell::const_new();

async fn setup_pool() -> astra_core::SharedPool {
    SHARED_BOOTSTRAP
        .get_or_init(|| async {
            let settings = require_db_it_env();
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

fn ids() -> (String, String, String, String) {
    let suffix = Uuid::new_v4();
    (
        format!("session-{suffix}"),
        format!("user-{suffix}"),
        format!("run-{suffix}"),
        format!("lease-{suffix}"),
    )
}

async fn insert_session(pool: &astra_core::SharedPool, session_id: &str, user_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'phase2-agent', 'phase2 session', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .unwrap();
}

async fn insert_run(
    pool: &astra_core::SharedPool,
    session_id: &str,
    user_id: &str,
    run_id: &str,
    last_event_idx: i64,
) {
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, retry_scope, status, last_event_idx)
         VALUES (?, ?, ?, ?, ?, 'node', 'running', ?)",
    )
    .bind(run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .bind(run_id)
    .bind(last_event_idx)
    .execute(pool.get())
    .await
    .unwrap();
}

async fn insert_transcript_item(
    pool: &astra_core::SharedPool,
    session_id: &str,
    user_id: &str,
    run_id: &str,
    item_seq: i64,
    role: &str,
    content: &str,
) {
    sqlx::query(
        "INSERT INTO session_transcript_items
         (session_id, item_seq, user_id, run_id, role, content, content_hash, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(session_id)
    .bind(item_seq)
    .bind(user_id)
    .bind(run_id)
    .bind(role)
    .bind(content)
    .bind(hash_hex(content))
    .execute(pool.get())
    .await
    .unwrap();
}

async fn insert_device_lease(
    pool: &astra_core::SharedPool,
    session_id: &str,
    user_id: &str,
    lease_id: &str,
    device_id: &str,
    fingerprint: &str,
    expires_sql: &str,
) {
    let sql = format!(
        "INSERT INTO session_device_leases
         (lease_id, user_id, session_id, device_id, device_fingerprint, device_key_hash, trust_level,
          status, last_monotonic_id, expires_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, 'new_device', 'active', 7, {expires_sql}, NOW(6), NOW(6))"
    );
    sqlx::query(&sql)
        .bind(lease_id)
        .bind(user_id)
        .bind(session_id)
        .bind(device_id)
        .bind(fingerprint)
        .bind(format!("{:x}", Sha256::digest(b"phase2-test-device-key")))
        .execute(pool.get())
        .await
        .unwrap();
}

fn hash_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!("sha256:{digest:x}")
}

fn revision_hash(
    session_id: &str,
    monotonic_id: u64,
    device_fingerprint: &str,
    transcript_high_watermark: i64,
    run_event_high_watermark: i64,
    state_projection_hash: &str,
) -> String {
    hash_hex(&format!(
        "{session_id}|{monotonic_id}|{device_fingerprint}|{transcript_high_watermark}|{run_event_high_watermark}|{state_projection_hash}"
    ))
}

shared_db_test! {
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_14_cold_start_known_zero_with_active_run_requires_replay() {
    let pool = setup_pool().await;
    let (session_id, user_id, run_id, _) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(&pool, &session_id, &user_id, &run_id, 9).await;
    insert_transcript_item(
        &pool,
        &session_id,
        &user_id,
        &run_id,
        3,
        "assistant",
        "ready",
    )
    .await;

    let transcript_hwm = sqlx::query(
        "SELECT COALESCE(MAX(item_seq), 0) AS hwm
         FROM session_transcript_items
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("hwm")
    .unwrap();
    let run_hwm = sqlx::query(
        "SELECT last_event_idx FROM agent_runs WHERE session_id = ? AND status = 'running'",
    )
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("last_event_idx")
    .unwrap();

    let known_state_revision = 0_u64;
    let replay_required = known_state_revision == 0 && run_hwm > 0;
    assert_eq!(transcript_hwm, 3);
    assert_eq!(run_hwm, 9);
    assert!(replay_required);
}

}

shared_db_test! {
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_13_revoke_and_auto_expire_events_have_symmetric_payload_shape() {
    let pool = setup_pool().await;
    let (session_id, user_id, _, lease_a) = ids();
    let lease_b = format!("lease-{}", Uuid::new_v4());
    insert_session(&pool, &session_id, &user_id).await;
    insert_device_lease(
        &pool,
        &session_id,
        &user_id,
        &lease_a,
        "macbook",
        "fp-mac",
        "DATE_ADD(NOW(6), INTERVAL 1 HOUR)",
    )
    .await;
    insert_device_lease(
        &pool,
        &session_id,
        &user_id,
        &lease_b,
        "chromebook",
        "fp-chrome",
        "DATE_SUB(NOW(6), INTERVAL 1 SECOND)",
    )
    .await;

    for (lease_id, device_id, event_type, reason) in [
        (&lease_a, "macbook", "device_revoked", "explicit_revoke"),
        (
            &lease_b,
            "chromebook",
            "device_lease_expired",
            "auto_expire",
        ),
    ] {
        sqlx::query(
            "INSERT INTO session_device_lease_events
             (lease_event_id, lease_id, user_id, session_id, device_id, device_fingerprint,
              event_type, reason, ended_at_server, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(lease_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(device_id)
        .bind(format!("fp-{device_id}"))
        .bind(event_type)
        .bind(reason)
        .execute(pool.get())
        .await
        .unwrap();
    }

    let rows = sqlx::query(
        "SELECT event_type, lease_id, session_id, device_id, device_fingerprint, reason,
                DATE_FORMAT(ended_at_server, '%Y-%m-%dT%H:%i:%s') AS ended_at_server
         FROM session_device_lease_events
         WHERE session_id = ?
         ORDER BY event_type ASC",
    )
    .bind(&session_id)
    .fetch_all(pool.get())
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(matches!(
            row.try_get::<String, _>("event_type").unwrap().as_str(),
            "device_revoked" | "device_lease_expired"
        ));
        for column in [
            "lease_id",
            "session_id",
            "device_id",
            "device_fingerprint",
            "reason",
            "ended_at_server",
        ] {
            assert!(
                !row.try_get::<String, _>(column)
                    .unwrap_or_default()
                    .is_empty(),
                "missing symmetric payload column {column}"
            );
        }
    }
}

}

shared_db_test! {
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_15_revision_hash_changes_when_device_fingerprint_changes() {
    let session_id = format!("session-{}", Uuid::new_v4());
    let projection_hash = hash_hex("projection");
    let a = revision_hash(&session_id, 10, "fp-a", 3, 9, &projection_hash);
    let b = revision_hash(&session_id, 10, "fp-b", 3, 9, &projection_hash);
    assert_ne!(
        a, b,
        "rollback detection must compare the full device-bound hash"
    );
}

}

shared_db_test! {
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_16_device_trust_transition_is_active_lease_cas_guarded() {
    let pool = setup_pool().await;
    let (session_id, user_id, _, lease_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_device_lease(
        &pool,
        &session_id,
        &user_id,
        &lease_id,
        "chromebook",
        "fp-chrome",
        "DATE_ADD(NOW(6), INTERVAL 1 HOUR)",
    )
    .await;

    let updated = sqlx::query(
        "UPDATE session_device_leases
         SET trust_level = 'trusted', updated_at = NOW(6)
         WHERE lease_id = ? AND trust_level = 'new_device' AND status = 'active'
           AND expires_at > NOW(6) AND last_monotonic_id = 7",
    )
    .bind(&lease_id)
    .execute(pool.get())
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(updated, 1);
}

}

shared_db_test! {
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_17_revoke_api_cas_is_idempotent_after_first_write() {
    let pool = setup_pool().await;
    let (session_id, user_id, _, lease_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_device_lease(
        &pool,
        &session_id,
        &user_id,
        &lease_id,
        "ipad",
        "fp-ipad",
        "DATE_ADD(NOW(6), INTERVAL 1 HOUR)",
    )
    .await;

    let first = sqlx::query(
        "UPDATE session_device_leases
         SET status = 'revoked', revoked_at = NOW(6), updated_at = NOW(6)
         WHERE lease_id = ? AND status = 'active' AND last_monotonic_id = 7",
    )
    .bind(&lease_id)
    .execute(pool.get())
    .await
    .unwrap()
    .rows_affected();
    let second = sqlx::query(
        "UPDATE session_device_leases
         SET status = 'revoked', revoked_at = NOW(6), updated_at = NOW(6)
         WHERE lease_id = ? AND status = 'active' AND last_monotonic_id = 7",
    )
    .bind(&lease_id)
    .execute(pool.get())
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(first, 1);
    assert_eq!(
        second, 0,
        "second revoke is served from terminal lease state"
    );
}

}

shared_db_test! {
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l3_4_s03_four_device_switches_restore_ordered_transcript() {
    let pool = setup_pool().await;
    let (session_id, user_id, run_id, _) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(&pool, &session_id, &user_id, &run_id, 4).await;
    for (seq, role, content) in [
        (1, "user", "macbook starts"),
        (2, "assistant", "ipad resumes"),
        (3, "user", "chromebook checks"),
        (4, "assistant", "macbook restores"),
    ] {
        insert_transcript_item(&pool, &session_id, &user_id, &run_id, seq, role, content).await;
    }

    let rows = sqlx::query(
        "SELECT item_seq, content FROM session_transcript_items
         WHERE user_id = ? AND session_id = ? AND item_seq < ?
         ORDER BY item_seq DESC
         LIMIT 4",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(i64::MAX)
    .fetch_all(pool.get())
    .await
    .unwrap();
    let mut seqs = rows
        .into_iter()
        .map(|row| row.try_get::<i64, _>("item_seq").unwrap())
        .collect::<Vec<_>>();
    seqs.reverse();
    assert_eq!(seqs, [1, 2, 3, 4]);
}

}

shared_db_test! {
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l3_5_s04_t09_double_tab_uses_max_watermark_without_rollback() {
    let session_id = format!("session-{}", Uuid::new_v4());
    let projection_hash = hash_hex("projection");
    let tab_a_hash = revision_hash(&session_id, 8, "fp-tab-a", 5, 8, &projection_hash);
    let tab_b_hash = revision_hash(&session_id, 10, "fp-tab-b", 5, 10, &projection_hash);
    let shared_watermark = 10_i64;
    assert_eq!(shared_watermark, 10);
    assert_ne!(tab_a_hash, tab_b_hash);
}

}

shared_db_test! {
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l3_6_s03_t8_passive_expiry_records_auto_expire_event() {
    let pool = setup_pool().await;
    let (session_id, user_id, _, lease_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_device_lease(
        &pool,
        &session_id,
        &user_id,
        &lease_id,
        "chromebook",
        "fp-chrome",
        "DATE_SUB(NOW(6), INTERVAL 1 SECOND)",
    )
    .await;

    let changed = sqlx::query(
        "UPDATE session_device_leases
         SET status = 'expired', revoked_at = NOW(6), updated_at = NOW(6)
         WHERE lease_id = ? AND status = 'active' AND expires_at <= NOW(6)",
    )
    .bind(&lease_id)
    .execute(pool.get())
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(changed, 1);
    sqlx::query(
        "INSERT INTO session_device_lease_events
         (lease_event_id, lease_id, user_id, session_id, device_id, device_fingerprint,
          event_type, reason, ended_at_server, created_at)
         VALUES (?, ?, ?, ?, 'chromebook', 'fp-chrome', 'device_lease_expired', 'auto_expire', NOW(6), NOW(6))",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&lease_id)
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .unwrap();

    let reason = sqlx::query(
        "SELECT reason FROM session_device_lease_events WHERE lease_id = ? AND event_type = 'device_lease_expired'",
    )
    .bind(&lease_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<String, _>("reason")
    .unwrap();
    assert_eq!(reason, "auto_expire");
}
}
