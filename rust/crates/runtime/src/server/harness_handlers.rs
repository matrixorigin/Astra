//! HTTP handlers for harness snapshot queries.
//!
//! `GET /sessions/{session_id}/harness/snapshot` — latest snapshot
//! `GET /sessions/{session_id}/harness/history?n=5` — snapshot history
//! `GET /sessions/{session_id}/harness/diff` — diff between last two snapshots

#[cfg(feature = "harness")]
pub use enabled::*;

#[cfg(feature = "harness")]
mod enabled {
    use astra_core::ErrorResponse;
    use astra_harness::{RuntimeSnapshot, SnapshotDiff, SnapshotSink};
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::Json;
    use dashmap::DashMap;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    use crate::app_state::AppState;

    /// Registry of active session harness sinks.
    /// Server loop hosts register their sink here; handlers read from it.
    #[derive(Clone, Default)]
    pub struct HarnessSinkRegistry {
        sinks: Arc<DashMap<String, Arc<dyn SnapshotSink>>>,
        broadcasters: Arc<DashMap<String, broadcast::Sender<RuntimeSnapshot>>>,
    }

    impl HarnessSinkRegistry {
        pub fn new() -> Self {
            Self {
                sinks: Arc::new(DashMap::new()),
                broadcasters: Arc::new(DashMap::new()),
            }
        }

        pub fn register(&self, session_id: String, sink: Arc<dyn SnapshotSink>) {
            self.sinks.insert(session_id, sink);
        }

        pub fn register_with_broadcast(
            &self,
            session_id: String,
            sink: Arc<dyn SnapshotSink>,
            broadcaster: broadcast::Sender<RuntimeSnapshot>,
        ) {
            self.sinks.insert(session_id.clone(), sink);
            self.broadcasters.insert(session_id, broadcaster);
        }

        pub fn unregister(&self, session_id: &str) {
            self.sinks.remove(session_id);
            self.broadcasters.remove(session_id);
        }

        pub fn get(&self, session_id: &str) -> Option<Arc<dyn SnapshotSink>> {
            self.sinks.get(session_id).map(|r| r.value().clone())
        }

        pub fn subscribe(&self, session_id: &str) -> Option<broadcast::Receiver<RuntimeSnapshot>> {
            self.broadcasters
                .get(session_id)
                .map(|r| r.value().subscribe())
        }

        pub fn active_sessions(&self) -> Vec<String> {
            self.sinks.iter().map(|r| r.key().clone()).collect()
        }
    }

    #[derive(serde::Deserialize)]
    pub struct HistoryParams {
        #[serde(default = "default_history_count")]
        pub n: usize,
    }

    fn default_history_count() -> usize {
        10
    }

    /// Strip sensitive fields for non-admin snapshot access.
    fn sanitize_snapshot(snap: RuntimeSnapshot) -> RuntimeSnapshot {
        RuntimeSnapshot {
            model: None,
            unique_tools_used: vec![],
            last_tool_called: None,
            ..snap
        }
    }

    async fn persisted_history(
        state: &AppState,
        session_id: &str,
        n: usize,
    ) -> Vec<RuntimeSnapshot> {
        if n == 0 {
            return Vec::new();
        }
        let Some(pool) = state.shared_pool.as_ref() else {
            return Vec::new();
        };
        let limit = n.min(1000) as i64;
        let rows: Vec<(String,)> = match sqlx::query_as(
            "SELECT snapshot_json
             FROM harness_snapshots
             WHERE session_id = ?
             ORDER BY created_at DESC
             LIMIT ?",
        )
        .bind(session_id)
        .bind(limit)
        .fetch_all(pool.get())
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(session_id, error = %e, "failed to read persisted harness snapshots");
                return Vec::new();
            }
        };

        rows.into_iter()
            .filter_map(|(json,)| serde_json::from_str::<RuntimeSnapshot>(&json).ok())
            .collect()
    }

    pub async fn get_harness_snapshot(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(session_id): Path<String>,
    ) -> Result<Json<RuntimeSnapshot>, (StatusCode, Json<ErrorResponse>)> {
        let user = state.auth_service.current_user(&headers).await?;
        state
            .session_service
            .get_session(session_id.clone(), user.user_id)
            .await?;
        let registry = &state.harness_registry;
        let snapshot = if let Some(sink) = registry.get(&session_id) {
            sink.latest()
        } else {
            None
        };
        let snapshot = match snapshot {
            Some(snapshot) => snapshot,
            None => persisted_history(&state, &session_id, 1)
                .await
                .into_iter()
                .next()
                .ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        Json(ErrorResponse::new("no snapshot available yet")),
                    )
                })?,
        };
        Ok(Json(sanitize_snapshot(snapshot)))
    }

    pub async fn get_harness_history(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(session_id): Path<String>,
        Query(params): Query<HistoryParams>,
    ) -> Result<Json<Vec<RuntimeSnapshot>>, (StatusCode, Json<ErrorResponse>)> {
        let user = state.auth_service.current_user(&headers).await?;
        state
            .session_service
            .get_session(session_id.clone(), user.user_id)
            .await?;
        let registry = &state.harness_registry;
        let mut history: Vec<RuntimeSnapshot> = registry
            .get(&session_id)
            .map(|sink| sink.history(params.n))
            .unwrap_or_default();
        if history.is_empty() {
            history = persisted_history(&state, &session_id, params.n).await;
        }
        let history: Vec<RuntimeSnapshot> = history.into_iter().map(sanitize_snapshot).collect();
        if history.is_empty() {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("no history available yet")),
            ));
        }
        Ok(Json(history))
    }

    /// SSE stream: pushes snapshots as they arrive via broadcast channel.
    pub async fn stream_harness_snapshots(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(session_id): Path<String>,
    ) -> axum::response::Response {
        use axum::body::Body;
        use axum::http::header;
        use axum::response::IntoResponse;

        let user = match state.auth_service.current_user(&headers).await {
            Ok(u) => u,
            Err(err) => return err.into_response(),
        };
        if state
            .session_service
            .get_session(session_id.clone(), user.user_id)
            .await
            .is_err()
        {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("session not found")),
            )
                .into_response();
        }

        let registry = state.harness_registry.clone();
        let mut rx = match registry.subscribe(&session_id) {
            Some(rx) => rx,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("session not found or no broadcast")),
                )
                    .into_response();
            }
        };

        let stream = async_stream::stream! {
            // Send seed snapshot and track its identity for dedup
            let mut seed_key: Option<(u64, u32)> = None;
            if let Some(sink) = registry.get(&session_id) {
                if let Some(snap) = sink.latest() {
                    seed_key = Some((snap.captured_at_unix_millis, snap.turn_number));
                    if let Ok(json) = serde_json::to_string(&snap) {
                        yield Ok::<_, std::convert::Infallible>(
                            format!("data: {json}\n\n")
                        );
                    }
                }
            }

            loop {
                match rx.recv().await {
                    Ok(snap) => {
                        // Skip duplicate of the seed snapshot
                        if let Some((seed_ts, seed_turn)) = seed_key {
                            if snap.captured_at_unix_millis <= seed_ts
                                && snap.turn_number <= seed_turn
                            {
                                continue;
                            }
                            seed_key = None;
                        }
                        if let Ok(json) = serde_json::to_string(&snap) {
                            yield Ok::<_, std::convert::Infallible>(
                                format!("data: {json}\n\n")
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "harness SSE subscriber lagged");
                        yield Ok::<_, std::convert::Infallible>(
                            format!("event: error\ndata: {{\"type\":\"lagged\",\"missed\":{n}}}\n\n")
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };

        let body = Body::from_stream(stream);
        axum::response::Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .header("x-accel-buffering", "no")
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }

    /// Admin overview: all active sessions with their latest snapshot.
    #[derive(serde::Serialize)]
    pub struct ActiveSessionSnapshot {
        pub session_id: String,
        pub snapshot: Option<RuntimeSnapshot>,
    }

    #[derive(serde::Deserialize)]
    pub struct AdminListParams {
        #[serde(default = "default_admin_limit")]
        pub limit: usize,
        #[serde(default)]
        pub offset: usize,
    }

    fn default_admin_limit() -> usize {
        100
    }

    pub async fn list_active_harness_sessions(
        State(state): State<AppState>,
        headers: HeaderMap,
        Query(params): Query<AdminListParams>,
    ) -> Result<Json<Vec<ActiveSessionSnapshot>>, (StatusCode, Json<ErrorResponse>)> {
        state.admin_authorizer.require_admin(&headers).await?;
        let registry = &state.harness_registry;
        let sessions = registry.active_sessions();
        let result: Vec<ActiveSessionSnapshot> = sessions
            .into_iter()
            .skip(params.offset)
            .take(params.limit)
            .map(|session_id| {
                let snapshot = registry.get(&session_id).and_then(|s| s.latest());
                ActiveSessionSnapshot {
                    session_id,
                    snapshot,
                }
            })
            .collect();
        Ok(Json(result))
    }

    pub async fn get_harness_diff(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(session_id): Path<String>,
    ) -> Result<Json<SnapshotDiff>, (StatusCode, Json<ErrorResponse>)> {
        let user = state.auth_service.current_user(&headers).await?;
        state
            .session_service
            .get_session(session_id.clone(), user.user_id)
            .await?;
        let registry = &state.harness_registry;
        let mut history = registry
            .get(&session_id)
            .map(|sink| sink.history(2))
            .unwrap_or_default();
        if history.len() < 2 {
            history = persisted_history(&state, &session_id, 2).await;
        }
        if history.len() < 2 {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("not enough snapshots for diff yet")),
            ));
        }
        Ok(Json(SnapshotDiff::between(&history[1], &history[0])))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use astra_harness::{DecisionRecord, HookPoint, InMemorySnapshotSink};

        fn populated_registry() -> HarnessSinkRegistry {
            let registry = HarnessSinkRegistry::new();
            let sink = InMemorySnapshotSink::arc();

            for i in 1..=3 {
                sink.update(&DecisionRecord {
                    session_id: "s1".into(),
                    turn: i,
                    point: HookPoint::PostTurn,
                    wall_time_unix_millis: i as u64 * 1000,
                    monotonic_millis_since_session: i as u64 * 1000,
                    snapshot: RuntimeSnapshot {
                        session_id: "s1".into(),
                        turn_number: i,
                        turns_used: i,
                        tokens_used_session: i as u64 * 5_000,
                        ..RuntimeSnapshot::empty()
                    },
                });
            }

            registry.register("s1".into(), sink as Arc<dyn SnapshotSink>);
            registry
        }

        #[test]
        fn registry_register_and_get() {
            let reg = populated_registry();
            assert!(reg.get("s1").is_some());
            assert!(reg.get("unknown").is_none());
        }

        #[test]
        fn registry_unregister() {
            let reg = populated_registry();
            reg.unregister("s1");
            assert!(reg.get("s1").is_none());
        }

        #[test]
        fn registry_active_sessions() {
            let reg = populated_registry();
            let sessions = reg.active_sessions();
            assert_eq!(sessions, vec!["s1"]);
        }

        #[test]
        fn registry_get_returns_working_sink() {
            let reg = populated_registry();
            let sink = reg.get("s1").unwrap();
            let snap = sink.latest().unwrap();
            assert_eq!(snap.turn_number, 3);
        }

        #[test]
        fn registry_snapshot_via_sink_returns_latest() {
            let reg = populated_registry();
            let sink = reg.get("s1").unwrap();
            let snap = sink.latest().unwrap();
            assert_eq!(snap.turn_number, 3);
        }

        #[test]
        fn registry_snapshot_none_for_unknown_session() {
            let reg = HarnessSinkRegistry::new();
            assert!(reg.get("unknown").is_none());
        }

        #[test]
        fn registry_history_returns_newest_first() {
            let reg = populated_registry();
            let sink = reg.get("s1").unwrap();
            let history = sink.history(2);
            assert_eq!(history.len(), 2);
            assert_eq!(history[0].turn_number, 3);
            assert_eq!(history[1].turn_number, 2);
        }

        #[test]
        fn registry_diff_computes_delta() {
            let reg = populated_registry();
            let sink = reg.get("s1").unwrap();
            let history = sink.history(2);
            assert!(history.len() >= 2);
            let diff = SnapshotDiff::between(&history[1], &history[0]);
            assert_eq!(diff.from_turn, 2);
            assert_eq!(diff.to_turn, 3);
            assert_eq!(diff.tokens_delta, 5_000);
        }

        #[test]
        fn registry_subscribe_returns_receiver() {
            let reg = HarnessSinkRegistry::new();
            let sink = InMemorySnapshotSink::arc();
            let (tx, _) = broadcast::channel::<RuntimeSnapshot>(16);
            reg.register_with_broadcast("s2".into(), sink as Arc<dyn SnapshotSink>, tx);
            assert!(reg.subscribe("s2").is_some());
            assert!(reg.subscribe("unknown").is_none());
        }

        #[test]
        fn registry_subscribe_receives_sent_snapshot() {
            let reg = HarnessSinkRegistry::new();
            let sink = InMemorySnapshotSink::arc();
            let (tx, _) = broadcast::channel::<RuntimeSnapshot>(16);
            reg.register_with_broadcast("s3".into(), sink as Arc<dyn SnapshotSink>, tx.clone());

            let mut rx = reg.subscribe("s3").unwrap();
            let snap = RuntimeSnapshot {
                turn_number: 42,
                ..RuntimeSnapshot::empty()
            };
            tx.send(snap).unwrap();

            let received = rx.try_recv().unwrap();
            assert_eq!(received.turn_number, 42);
        }

        #[test]
        fn registry_unregister_cleans_broadcaster() {
            let reg = HarnessSinkRegistry::new();
            let sink = InMemorySnapshotSink::arc();
            let (tx, _) = broadcast::channel::<RuntimeSnapshot>(16);
            reg.register_with_broadcast("s4".into(), sink as Arc<dyn SnapshotSink>, tx);
            reg.unregister("s4");
            assert!(reg.subscribe("s4").is_none());
        }

        #[test]
        fn registry_active_sessions_lists_all() {
            let reg = populated_registry();
            let sessions = reg.active_sessions();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0], "s1");
            // Verify the sink is accessible for each listed session
            for sid in &sessions {
                assert!(reg.get(sid).is_some());
            }
        }

        #[test]
        fn registry_active_sessions_empty_when_no_sessions() {
            let reg = HarnessSinkRegistry::new();
            let sessions = reg.active_sessions();
            assert!(sessions.is_empty());
        }

        #[test]
        fn sanitize_snapshot_strips_sensitive_fields() {
            let snap = RuntimeSnapshot {
                model: Some("claude-sonnet-4-6".into()),
                unique_tools_used: vec!["bash".into(), "read_file".into()],
                last_tool_called: Some("bash".into()),
                turn_number: 5,
                tokens_used_session: 10_000,
                ..RuntimeSnapshot::empty()
            };
            let sanitized = sanitize_snapshot(snap);
            assert!(sanitized.model.is_none());
            assert!(sanitized.unique_tools_used.is_empty());
            assert!(sanitized.last_tool_called.is_none());
            // Non-sensitive fields preserved
            assert_eq!(sanitized.turn_number, 5);
            assert_eq!(sanitized.tokens_used_session, 10_000);
        }
    }
}
