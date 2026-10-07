//! Short, identical-source comparison across revisions; not a capacity claim.
mod common;

use astra_services::event_ingestion::{EventIngestionWorker, IngestionConfig, IngestionEvent};
use sqlx::Row;
use std::time::{Duration, Instant};

#[tokio::test]
#[ignore = "explicit disposable MatrixOne database; short revision comparison"]
async fn batch_tradeoff_probe() {
    let settings = common::require_db_it_env();
    assert!(settings.database.starts_with("astra_test_probe_"));
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    for sessions in [1_000, 10] {
        for repeat in 0..3 {
            let prefix = format!("batch-tradeoff-{}", uuid::Uuid::new_v4());
            let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO agent_sessions (session_id,user_id,title,status,event_count) ",
            );
            insert.push_values(0..sessions, |mut row, index| {
                row.push_bind(format!("{prefix}-s{index}"))
                    .push_bind(format!("{prefix}-u{}", index % 100))
                    .push_bind("synthetic batch comparison")
                    .push_bind("active")
                    .push_bind(0_i64);
            });
            insert.build().execute(&pool).await.unwrap();
            let (sender, shutdown, stats, worker) =
                EventIngestionWorker::spawn(pool.clone(), IngestionConfig::default());
            let started = Instant::now();
            let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let foreground = {
                let pool = pool.clone();
                let finished = finished.clone();
                tokio::spawn(async move {
                    let mut timings = Vec::new();
                    while !finished.load(std::sync::atomic::Ordering::Acquire) {
                        let start = Instant::now();
                        let _: i64 = sqlx::query_scalar("SELECT 1")
                            .fetch_one(&pool)
                            .await
                            .unwrap();
                        timings.push(start.elapsed().as_micros() as u64);
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    timings.sort_unstable();
                    (
                        timings.len(),
                        timings.get(timings.len() * 95 / 100).copied(),
                    )
                })
            };
            for index in 0..1_000 {
                let session = index % sessions;
                // Deserialize the common wire fields so this exact source also
                // builds before private scheduler fields were added.
                let event: IngestionEvent = serde_json::from_value(serde_json::json!({
                    "event_id":format!("{prefix}-e{index}"),
                    "session_id":format!("{prefix}-s{session}"),
                    "user_id":format!("{prefix}-u{}",session%100),
                    "event_type":"user_query", "content":"x".repeat(256),
                    "created_at":"2025-01-15T10:30:00Z", "parent_event_ids":[]
                }))
                .unwrap();
                sender.enqueue(event);
            }
            let mut first_visible = None;
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let count: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM agent_events WHERE user_id LIKE ?",
                    )
                    .bind(format!("{prefix}-u%"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    if count > 0 {
                        first_visible.get_or_insert(started.elapsed());
                    }
                    if count == 1_000 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("bounded finite workload");
            let all_visible = started.elapsed();
            finished.store(true, std::sync::atomic::Ordering::Release);
            shutdown.signal();
            sender.shutdown();
            tokio::time::timeout(Duration::from_secs(5), worker)
                .await
                .unwrap()
                .unwrap();
            let (foreground_samples, foreground_p95_us) = foreground.await.unwrap();
            let resolved_flushes = {
                let stats = astra_core::sync_poison::recover_mutex_lock(&stats);
                assert_eq!(stats.events_flushed, 1_000);
                assert_eq!(stats.errors, 0);
                stats.flush_count
            };
            let rows = sqlx::query("SELECT event_count FROM agent_sessions WHERE user_id LIKE ?")
                .bind(format!("{prefix}-u%"))
                .fetch_all(&pool)
                .await
                .unwrap();
            assert_eq!(rows.len(), sessions);
            assert!(
                rows.iter()
                    .all(|row| row.get::<i64, _>("event_count") == (1_000 / sessions) as i64)
            );
            println!(
                "BATCH_TRADEOFF {}",
                serde_json::json!({
                    "sessions":sessions,"events":1000,"repeat":repeat,"pool_size":pool.options().get_max_connections(),
                    "batch_size":IngestionConfig::default().batch_size,
                    "first_visible_us":first_visible.unwrap().as_micros(),"all_visible_us":all_visible.as_micros(),
                "resolved_flushes":resolved_flushes,"foreground_select1_p95_us":foreground_p95_us,
                "foreground_samples":foreground_samples,
                    "scope":"finite burst, test profile, no contention injection"
                })
            );
            for table in ["agent_events", "agent_sessions"] {
                sqlx::query(&format!("DELETE FROM {table} WHERE user_id LIKE ?"))
                    .bind(format!("{prefix}-u%"))
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
    }
}
