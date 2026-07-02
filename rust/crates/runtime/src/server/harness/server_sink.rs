//! In-memory + SSE broadcast snapshot sink for web/server agent sessions.
//!
//! The default path keeps snapshots in memory and broadcasts them via tokio
//! channel so SSE subscribers receive real-time harness updates. Durable DB
//! persistence is opt-in because every hook snapshot is observability data, not
//! part of the run's success path.

#[cfg(feature = "harness")]
pub use enabled::ServerSnapshotSink;

#[cfg(feature = "harness")]
mod enabled {
    use astra_harness::{DecisionRecord, RuntimeSnapshot, SnapshotSink};
    use serde::Serialize;
    use std::collections::VecDeque;
    use std::sync::{OnceLock, RwLock};
    use tokio::sync::broadcast;

    const DEFAULT_IN_MEMORY_CAPACITY: usize = 64;
    const DEFAULT_BROADCAST_CAPACITY: usize = 256;

    const DB_WRITE_CHANNEL_CAPACITY: usize = 64;
    pub const ASTRA_HARNESS_SNAPSHOT_DB_PERSIST_ENV: &str = "ASTRA_HARNESS_SNAPSHOT_DB_PERSIST";
    static HARNESS_SNAPSHOT_DB_PERSIST_ENABLED: OnceLock<bool> = OnceLock::new();

    struct DbWriteTask {
        snapshot_id: String,
        session_id: String,
        user_id: String,
        hook_point: String,
        turn_number: u32,
        snapshot_json: String,
        causal_chain_id: Option<String>,
    }

    /// Server-side snapshot sink: in-memory ring + broadcast + optional bounded DB worker.
    ///
    /// The in-memory ring provides fast `latest()` / `history()` queries.
    /// The broadcast channel pushes snapshots to SSE subscribers.
    /// DB persistence is disabled by default. When explicitly enabled, it uses
    /// a bounded mpsc channel with a single worker task to prevent unbounded
    /// spawn storms under load.
    pub struct ServerSnapshotSink {
        session_id: String,
        user_id: RwLock<String>,
        ring: RwLock<VecDeque<RuntimeSnapshot>>,
        ring_capacity: usize,
        broadcaster: broadcast::Sender<RuntimeSnapshot>,
        db_tx: Option<tokio::sync::mpsc::Sender<DbWriteTask>>,
        /// Retained so the background DB writer task is not detached.
        /// The runtime's graceful shutdown drains the mpsc channel, which
        /// causes the worker to exit; holding the handle lets callers
        /// `await` it if needed.
        _db_worker: Option<tokio::task::JoinHandle<()>>,
        /// Count of snapshots dropped before DB write due to serialization
        /// failure or queue backpressure.
        dropped_writes: std::sync::atomic::AtomicU64,
    }

    impl ServerSnapshotSink {
        pub fn new(session_id: String, user_id: String) -> Self {
            let (tx, _) = broadcast::channel(DEFAULT_BROADCAST_CAPACITY);
            Self {
                session_id,
                user_id: RwLock::new(user_id),
                ring: RwLock::new(VecDeque::with_capacity(DEFAULT_IN_MEMORY_CAPACITY)),
                ring_capacity: DEFAULT_IN_MEMORY_CAPACITY,
                broadcaster: tx,
                db_tx: None,
                _db_worker: None,
                dropped_writes: std::sync::atomic::AtomicU64::new(0),
            }
        }

        /// Set the user_id after construction (e.g. when user_id is not
        /// available at sink creation time but becomes available later).
        pub fn set_user_id(&self, user_id: String) {
            if let Ok(mut guard) = self.user_id.write() {
                *guard = user_id;
            }
        }

        pub fn with_pool(self, pool: sqlx::Pool<sqlx::MySql>) -> Self {
            self.with_pool_if_enabled(pool, harness_snapshot_db_persist_enabled_from_env())
        }

        fn with_pool_if_enabled(
            mut self,
            pool: sqlx::Pool<sqlx::MySql>,
            persistence_enabled: bool,
        ) -> Self {
            if !persistence_enabled {
                tracing::debug!(
                    session_id = %self.session_id,
                    env = ASTRA_HARNESS_SNAPSHOT_DB_PERSIST_ENV,
                    "harness snapshot DB persistence disabled"
                );
                return self;
            }
            let (tx, rx) = tokio::sync::mpsc::channel(DB_WRITE_CHANNEL_CAPACITY);
            let handle = tokio::spawn(db_write_worker(rx, pool));
            self.db_tx = Some(tx);
            self._db_worker = Some(handle);
            self
        }

        pub fn db_persistence_is_active(&self) -> bool {
            self.db_tx.is_some()
        }

        /// Number of snapshot writes dropped before DB persistence.
        pub fn dropped_write_count(&self) -> u64 {
            self.dropped_writes
                .load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Subscribe to live snapshot updates.
        pub fn subscribe(&self) -> broadcast::Receiver<RuntimeSnapshot> {
            self.broadcaster.subscribe()
        }

        /// Get a clone of the broadcast sender for registry registration.
        pub fn broadcaster_sender(&self) -> broadcast::Sender<RuntimeSnapshot> {
            self.broadcaster.clone()
        }

        /// Number of active SSE subscribers.
        pub fn subscriber_count(&self) -> usize {
            self.broadcaster.receiver_count()
        }

        fn push_ring(&self, snap: &RuntimeSnapshot) {
            match self.ring.write() {
                Ok(mut ring) => {
                    if ring.len() >= self.ring_capacity {
                        ring.pop_front();
                    }
                    ring.push_back(snap.clone());
                }
                Err(poison) => {
                    tracing::error!("ServerSnapshotSink ring lock poisoned — recovering");
                    let mut ring = poison.into_inner();
                    if ring.len() >= self.ring_capacity {
                        ring.pop_front();
                    }
                    ring.push_back(snap.clone());
                }
            }
        }

        fn persist_to_db(&self, record: &DecisionRecord) {
            let Some(ref tx) = self.db_tx else { return };
            let user_id = self
                .user_id
                .read()
                .ok()
                .map(|g| g.clone())
                .unwrap_or_default();
            let snapshot_json = match serialize_snapshot_json(&record.snapshot) {
                Ok(json) => json,
                Err(e) => {
                    self.dropped_writes
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        dropped_total = self.dropped_writes.load(std::sync::atomic::Ordering::Relaxed),
                        "harness snapshot serialization failed, DB write dropped"
                    );
                    return;
                }
            };
            let task = DbWriteTask {
                snapshot_id: uuid::Uuid::now_v7().to_string(),
                session_id: self.session_id.clone(),
                user_id,
                hook_point: format!("{:?}", record.point),
                turn_number: record.snapshot.turn_number,
                snapshot_json,
                causal_chain_id: record.snapshot.causal_chain_id.clone(),
            };
            if let Err(e) = tx.try_send(task) {
                self.dropped_writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    session_id = %self.session_id,
                    dropped_total = self.dropped_writes.load(std::sync::atomic::Ordering::Relaxed),
                    "harness DB write queue full, snapshot dropped: {e}"
                );
            }
        }
    }

    fn serialize_snapshot_json<T: Serialize>(snapshot: &T) -> Result<String, String> {
        serde_json::to_string(snapshot)
            .map_err(|source| format!("serialize harness snapshot: {source}"))
    }

    fn harness_snapshot_db_persist_enabled_from_env() -> bool {
        *HARNESS_SNAPSHOT_DB_PERSIST_ENABLED.get_or_init(|| {
            harness_snapshot_db_persist_enabled(
                std::env::var(ASTRA_HARNESS_SNAPSHOT_DB_PERSIST_ENV)
                    .ok()
                    .as_deref(),
            )
        })
    }

    fn harness_snapshot_db_persist_enabled(raw: Option<&str>) -> bool {
        match raw.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value)
                if value.eq_ignore_ascii_case("1")
                    || value.eq_ignore_ascii_case("true")
                    || value.eq_ignore_ascii_case("yes")
                    || value.eq_ignore_ascii_case("on")
                    || value.eq_ignore_ascii_case("enabled") =>
            {
                true
            }
            Some(value)
                if value.eq_ignore_ascii_case("0")
                    || value.eq_ignore_ascii_case("false")
                    || value.eq_ignore_ascii_case("no")
                    || value.eq_ignore_ascii_case("off")
                    || value.eq_ignore_ascii_case("disabled") =>
            {
                false
            }
            Some(value) => {
                tracing::warn!(
                    env = ASTRA_HARNESS_SNAPSHOT_DB_PERSIST_ENV,
                    value,
                    "invalid harness snapshot DB persistence value; using disabled"
                );
                false
            }
            None => false,
        }
    }

    async fn db_write_worker(
        mut rx: tokio::sync::mpsc::Receiver<DbWriteTask>,
        pool: sqlx::Pool<sqlx::MySql>,
    ) {
        while let Some(task) = rx.recv().await {
            if let Err(e) = sqlx::query(
                "INSERT INTO harness_snapshots \
                 (snapshot_id, session_id, user_id, hook_point, turn_number, snapshot_json, causal_chain_id, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6))",
            )
            .bind(&task.snapshot_id)
            .bind(&task.session_id)
            .bind(&task.user_id)
            .bind(&task.hook_point)
            .bind(task.turn_number)
            .bind(&task.snapshot_json)
            .bind(&task.causal_chain_id)
            .execute(&pool)
            .await
            {
                tracing::warn!(
                    session_id = %task.session_id,
                    error = %e,
                    "harness snapshot DB persist failed"
                );
            }
        }
    }

    impl SnapshotSink for ServerSnapshotSink {
        fn update(&self, record: &DecisionRecord) {
            self.push_ring(&record.snapshot);
            if self.broadcaster.send(record.snapshot.clone()).is_err() {
                tracing::trace!(session_id = %self.session_id, "no SSE subscribers");
            }
            self.persist_to_db(record);
        }

        fn latest(&self) -> Option<RuntimeSnapshot> {
            self.ring.read().ok().and_then(|r| r.back().cloned())
        }

        fn history(&self, n: usize) -> Vec<RuntimeSnapshot> {
            self.ring
                .read()
                .ok()
                .map(|r| r.iter().rev().take(n).cloned().collect())
                .unwrap_or_default()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use astra_harness::{HookPoint, RuntimeSnapshot};
        use serde::Serializer;

        struct FailingSerialize;

        impl Serialize for FailingSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                Err(serde::ser::Error::custom(
                    "forced snapshot serializer failure",
                ))
            }
        }

        fn make_record(turn: u32) -> DecisionRecord {
            DecisionRecord {
                session_id: "srv-test".into(),
                turn,
                point: HookPoint::PostTurn,
                wall_time_unix_millis: turn as u64 * 1000,
                monotonic_millis_since_session: turn as u64 * 1000,
                snapshot: RuntimeSnapshot {
                    session_id: "srv-test".into(),
                    turn_number: turn,
                    turns_used: turn,
                    tokens_used_session: turn as u64 * 5_000,
                    ..RuntimeSnapshot::empty()
                },
            }
        }

        #[test]
        fn snapshot_json_serialization_fails_loudly() {
            let err = serialize_snapshot_json(&FailingSerialize)
                .expect_err("snapshot serialization errors must be surfaced");

            assert!(
                err.contains("serialize harness snapshot")
                    && err.contains("forced snapshot serializer failure"),
                "snapshot serialization error should retain context: {err}"
            );
        }

        #[test]
        fn snapshot_json_serialization_round_trips_runtime_snapshot() {
            let record = make_record(7);
            let json = serialize_snapshot_json(&record.snapshot).expect("serialize snapshot");

            assert!(!json.is_empty(), "snapshot_json must not default to empty");
            let restored =
                serde_json::from_str::<RuntimeSnapshot>(&json).expect("snapshot should round trip");
            assert_eq!(restored.session_id, "srv-test");
            assert_eq!(restored.turn_number, 7);
        }

        #[test]
        fn latest_returns_most_recent() {
            let sink = ServerSnapshotSink::new("s1".into(), "test-user".into());
            assert!(sink.latest().is_none());

            sink.update(&make_record(1));
            sink.update(&make_record(2));
            sink.update(&make_record(3));

            let snap = sink.latest().unwrap();
            assert_eq!(snap.turn_number, 3);
        }

        #[test]
        fn history_returns_newest_first() {
            let sink = ServerSnapshotSink::new("s1".into(), "test-user".into());
            for i in 1..=5 {
                sink.update(&make_record(i));
            }

            let history = sink.history(3);
            assert_eq!(history.len(), 3);
            assert_eq!(history[0].turn_number, 5);
            assert_eq!(history[2].turn_number, 3);
        }

        #[test]
        fn broadcast_delivers_to_subscriber() {
            let sink = ServerSnapshotSink::new("s1".into(), "test-user".into());
            let mut rx = sink.subscribe();

            sink.update(&make_record(1));

            let snap = rx.try_recv().unwrap();
            assert_eq!(snap.turn_number, 1);
        }

        #[test]
        fn subscriber_count_tracks_active_receivers() {
            let sink = ServerSnapshotSink::new("s1".into(), "test-user".into());
            assert_eq!(sink.subscriber_count(), 0);

            let _rx1 = sink.subscribe();
            assert_eq!(sink.subscriber_count(), 1);

            let _rx2 = sink.subscribe();
            assert_eq!(sink.subscriber_count(), 2);

            drop(_rx1);
            // Note: broadcast doesn't immediately decrement on drop
            // but subscriber_count reflects the send-side view
        }

        #[test]
        fn no_pool_skips_db_persist() {
            // Should not panic or error without a pool
            let sink = ServerSnapshotSink::new("s1".into(), "test-user".into());
            sink.update(&make_record(1));
            assert_eq!(sink.latest().unwrap().turn_number, 1);
        }

        #[test]
        fn ring_bounded_eviction() {
            let mut sink = ServerSnapshotSink::new("s1".into(), "test-user".into());
            sink.ring_capacity = 3;

            for i in 1..=5 {
                sink.update(&make_record(i));
            }

            let history = sink.history(10);
            assert_eq!(history.len(), 3);
            assert_eq!(history[0].turn_number, 5);
            assert_eq!(history[2].turn_number, 3);
        }

        #[test]
        fn dropped_write_count_starts_at_zero() {
            let sink = ServerSnapshotSink::new("s1".into(), "test-user".into());
            assert_eq!(sink.dropped_write_count(), 0);
        }

        #[test]
        fn db_persistence_is_inactive_without_pool() {
            let sink = ServerSnapshotSink::new("s1".into(), "test-user".into());
            assert!(!sink.db_persistence_is_active());
        }

        #[tokio::test]
        async fn disabled_db_persistence_policy_does_not_start_worker_with_pool() {
            let pool = sqlx::mysql::MySqlPoolOptions::new()
                .connect_lazy("mysql://root:pass@127.0.0.1:1/astra_runtime")
                .expect("lazy MySQL pool should not connect");
            let sink = ServerSnapshotSink::new("s1".into(), "test-user".into())
                .with_pool_if_enabled(pool, false);

            assert!(!sink.db_persistence_is_active());
        }

        #[test]
        fn harness_snapshot_db_persist_env_defaults_to_disabled() {
            assert!(!harness_snapshot_db_persist_enabled(None));
            assert!(!harness_snapshot_db_persist_enabled(Some("")));
            assert!(!harness_snapshot_db_persist_enabled(Some("disabled")));
            assert!(!harness_snapshot_db_persist_enabled(Some("0")));
            assert!(!harness_snapshot_db_persist_enabled(Some("false")));
        }

        #[test]
        fn harness_snapshot_db_persist_env_accepts_enabled_values() {
            assert!(harness_snapshot_db_persist_enabled(Some("1")));
            assert!(harness_snapshot_db_persist_enabled(Some("true")));
            assert!(harness_snapshot_db_persist_enabled(Some("yes")));
            assert!(harness_snapshot_db_persist_enabled(Some("on")));
            assert!(harness_snapshot_db_persist_enabled(Some("enabled")));
        }

        #[test]
        fn harness_snapshot_db_persist_env_invalid_values_disable_persistence() {
            assert!(!harness_snapshot_db_persist_enabled(Some("maybe")));
        }
    }
}
