use super::*;

pub mod side_effects;

use self::side_effects::{
    build_bridge_response_guard_error_event, build_bridge_response_guard_side_effect_payloads,
    build_bridge_side_effect_payloads, dispatch_bridge_side_effect_request,
    run_bridge_hook_side_effects, sync_bridge_state_event, take_bridge_explain_event,
    take_bridge_prompt_fingerprints, take_bridge_side_effect_inputs, take_bridge_tail_update_args,
    take_bridge_warning_event,
};

pub mod circuit_breaker;
pub mod rate_limit_cooldown;
pub mod sse_events;

use self::circuit_breaker::{BridgeHealthMetrics, CircuitBreaker};
pub use self::rate_limit_cooldown::{
    CooldownReason, PerModelCooldown, RateLimitAction, RateLimitCooldown, RateLimitMetrics,
    RateLimitState,
};

/// Header allow-list predicate: only `x-mo-*` and `authorization` headers
/// are forwarded to the upstream bridge.
pub(crate) fn is_allowed_bridge_header(name: &str) -> bool {
    name.starts_with("x-mo-") || name == "authorization"
}
use self::sse_events::{
    StreamEventCursor, add_stream_event_metadata, bridge_state_tool_signatures,
    build_cloud_loop_progress_event_from_frame, build_cloud_tool_result_event_from_frame,
    build_error_event_from_frame, build_reasoning_delta_event_from_frame,
    build_text_delta_event_from_frame, build_token_usage_from_usage_event,
    build_tool_call_event_from_frame, build_tool_call_start_event_from_frame,
    build_tool_result_quality_event_from_frame, build_turn_complete_event_from_bridge_state,
    build_usage_event_from_frame, find_sse_frame_end, is_explain_frame, is_session_info_frame,
    is_turn_complete_frame, is_warning_frame, parse_bridge_state_frame, parse_sse_json_frame,
    parse_stream_event_id, render_sse_json, render_stream_event_id,
};

use crate::turn::routing::max_tool_rounds;
use astra_core::SharedPool;
use sqlx::{MySql, QueryBuilder, Row, query};
#[cfg(test)]
use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

const TOOL_RESULT_AUDIT_CHARS: usize = 4000;
/// Safety limit for SSE frame buffer. Prevents OOM if a client is slow or a
/// response is unexpectedly large. 16 MB accommodates any realistic SSE stream.
const MAX_SSE_BUFFER_BYTES: usize = 16 * 1024 * 1024;
/// Cap buffered non-SSE error bodies so upstream 5xx/JSON responses cannot
/// force unbounded memory growth while we normalize them into a local SSE error.
const MAX_BRIDGE_ERROR_BODY_BYTES: usize = 16 * 1024;
const BRIDGE_REPLAY_WINDOW_PREFIX: &str = "__bridge_replay__";
const BRIDGE_REPLAY_WINDOW_MAX_FRAMES: usize = 128;
const BRIDGE_REPLAY_WINDOW_MAX_BYTES: usize = 256 * 1024;
const BRIDGE_PERSISTED_REPLAY_SCOPE_LIMIT: usize = 16;
const BRIDGE_PERSISTED_REPLAY_INCOMPLETE_FLUSH_EVERY: u64 = 8;

fn synthesized_session_info_event(session_id: &str, run_id: Option<&str>) -> serde_json::Value {
    let mut event = serde_json::json!({
        "type": "session_info",
        "session_id": session_id,
    });
    if let Some(run_id) = run_id
        && let Some(obj) = event.as_object_mut()
    {
        obj.insert(
            "run_id".to_string(),
            serde_json::Value::String(run_id.to_string()),
        );
    }
    event
}

fn render_stream_event_bytes(
    mut event: serde_json::Value,
    next_sequence: &mut u64,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) -> Bytes {
    *next_sequence = next_sequence.saturating_add(1);
    add_stream_event_metadata(
        &mut event,
        *next_sequence,
        session_id,
        turn_chain_id,
        user_query_event_id,
    );
    Bytes::from(render_sse_json(event))
}

fn rewrite_sse_frame_with_stream_metadata(
    frame: &[u8],
    next_sequence: &mut u64,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) -> Bytes {
    match parse_sse_json_frame(frame) {
        Some(event) => render_stream_event_bytes(
            event,
            next_sequence,
            session_id,
            turn_chain_id,
            user_query_event_id,
        ),
        None => Bytes::copy_from_slice(frame),
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct BridgeReplayWindow {
    #[serde(default)]
    frames: Vec<String>,
    #[serde(default)]
    complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_persisted_sequence: Option<u64>,
}

#[derive(Debug, Clone)]
struct BridgeReplaySuffix {
    frames: Vec<Bytes>,
    last_cursor: StreamEventCursor,
    complete: bool,
}

impl BridgeReplayWindow {
    fn from_cache_entry(entry: serde_json::Map<String, serde_json::Value>) -> Self {
        serde_json::from_value(serde_json::Value::Object(entry)).unwrap_or_default()
    }

    fn into_cache_entry(self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::to_value(self)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    }

    fn append_frame(&mut self, frame: &[u8]) -> bool {
        let Some(cursor) = stream_cursor_from_frame(frame) else {
            return false;
        };
        let Some(text) = std::str::from_utf8(frame).ok() else {
            return false;
        };

        if let Some(overlap_index) = self.frames.iter().position(|existing| {
            stream_cursor_from_frame(existing.as_bytes())
                .is_some_and(|existing_cursor| existing_cursor.sequence >= cursor.sequence)
        }) {
            self.frames.truncate(overlap_index);
        }
        if self
            .last_persisted_sequence
            .is_some_and(|sequence| sequence >= cursor.sequence)
        {
            self.last_persisted_sequence = None;
        }

        self.frames.push(text.to_string());
        while self.frames.len() > BRIDGE_REPLAY_WINDOW_MAX_FRAMES
            || replay_window_total_bytes(&self.frames) > BRIDGE_REPLAY_WINDOW_MAX_BYTES
        {
            self.frames.remove(0);
        }
        self.complete = is_turn_complete_frame(frame);
        true
    }

    fn last_cursor(&self) -> Option<StreamEventCursor> {
        self.frames
            .last()
            .and_then(|frame| stream_cursor_from_frame(frame.as_bytes()))
    }

    fn should_persist(&self) -> bool {
        let Some(last_cursor) = self.last_cursor() else {
            return false;
        };
        if self.complete || self.last_persisted_sequence.is_none() {
            return true;
        }
        last_cursor.sequence
            >= self
                .last_persisted_sequence
                .unwrap_or_default()
                .saturating_add(BRIDGE_PERSISTED_REPLAY_INCOMPLETE_FLUSH_EVERY)
    }

    fn mark_persisted(&mut self) {
        self.last_persisted_sequence = self.last_cursor().map(|cursor| cursor.sequence);
    }

    fn suffix_after(
        &self,
        cursor: &StreamEventCursor,
        allow_incomplete: bool,
    ) -> Option<BridgeReplaySuffix> {
        if (!allow_incomplete && !self.complete) || self.frames.is_empty() {
            return None;
        }
        let parsed = self
            .frames
            .iter()
            .map(|frame| stream_cursor_from_frame(frame.as_bytes()).map(|event| (event, frame)))
            .collect::<Option<Vec<_>>>()?;
        let first_sequence = parsed.first()?.0.sequence;
        let last_sequence = parsed.last()?.0.sequence;
        if cursor.sequence + 1 < first_sequence || cursor.sequence > last_sequence {
            return None;
        }
        let replayed: Vec<(StreamEventCursor, &String)> = parsed
            .into_iter()
            .filter(|(event_cursor, _)| event_cursor.sequence > cursor.sequence)
            .collect();
        let last_cursor = replayed.last()?.0.clone();
        Some(BridgeReplaySuffix {
            frames: replayed
                .into_iter()
                .map(|(_, frame)| Bytes::copy_from_slice(frame.as_bytes()))
                .collect(),
            last_cursor,
            complete: self.complete,
        })
    }
}

#[async_trait]
pub(crate) trait BridgeReplayWindowStore: Send + Sync {
    async fn load_latest_window(
        &self,
        session_id: &str,
        scope_key: &str,
    ) -> Result<Option<BridgeReplayWindow>, String>;

    async fn persist_latest_window(
        &self,
        session_id: &str,
        scope_key: &str,
        window: &BridgeReplayWindow,
    ) -> Result<(), String>;
}

#[derive(Clone, Debug)]
pub(crate) struct DatabaseBridgeReplayWindowStore {
    pool: SharedPool,
}

impl DatabaseBridgeReplayWindowStore {
    pub(crate) fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    async fn load_legacy_latest_window(
        &self,
        pool: &sqlx::Pool<MySql>,
        session_id: &str,
        scope_key: &str,
    ) -> Result<Option<BridgeReplayWindow>, String> {
        let row = query(
            "SELECT scope_key, replay_window_json \
             FROM agent_session_replay_windows \
             WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| error.to_string())?;

        let Some(row) = row else {
            return Ok(None);
        };

        let stored_scope_key: String = row
            .try_get("scope_key")
            .map_err(|error| error.to_string())?;
        if stored_scope_key != scope_key {
            return Ok(None);
        }

        let replay_window_json: String = row
            .try_get("replay_window_json")
            .map_err(|error| error.to_string())?;
        let window =
            serde_json::from_str::<BridgeReplayWindow>(&replay_window_json).map_err(|error| {
                format!("invalid persisted replay window for session {session_id}: {error}")
            })?;
        Ok(Some(window))
    }

    async fn trim_scope_windows(
        &self,
        pool: &sqlx::Pool<MySql>,
        session_id: &str,
    ) -> Result<(), String> {
        let rows = query(
            "SELECT scope_key \
             FROM agent_session_replay_scope_windows \
             WHERE session_id = ? \
             ORDER BY updated_at DESC, scope_key DESC",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await
        .map_err(|error| error.to_string())?;
        let stale_scope_keys: Vec<String> = rows
            .into_iter()
            .skip(BRIDGE_PERSISTED_REPLAY_SCOPE_LIMIT)
            .filter_map(|row| row.try_get("scope_key").ok())
            .collect();
        if stale_scope_keys.is_empty() {
            return Ok(());
        }

        let mut builder = QueryBuilder::<MySql>::new(
            "DELETE FROM agent_session_replay_scope_windows WHERE session_id = ",
        );
        builder.push_bind(session_id);
        builder.push(" AND scope_key IN (");
        {
            let mut separated = builder.separated(", ");
            for scope_key in &stale_scope_keys {
                separated.push_bind(scope_key);
            }
        }
        builder.push(")");
        builder
            .build()
            .execute(pool)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[async_trait]
impl BridgeReplayWindowStore for DatabaseBridgeReplayWindowStore {
    async fn load_latest_window(
        &self,
        session_id: &str,
        scope_key: &str,
    ) -> Result<Option<BridgeReplayWindow>, String> {
        let pool = self.pool.get().clone();
        let row = query(
            "SELECT replay_window_json \
             FROM agent_session_replay_scope_windows \
             WHERE session_id = ? AND scope_key = ?",
        )
        .bind(session_id)
        .bind(scope_key)
        .fetch_optional(&pool)
        .await
        .map_err(|error| error.to_string())?;

        if let Some(row) = row {
            let replay_window_json: String = row
                .try_get("replay_window_json")
                .map_err(|error| error.to_string())?;
            let window = serde_json::from_str::<BridgeReplayWindow>(&replay_window_json).map_err(
                |error| {
                    format!("invalid persisted replay window for session {session_id}: {error}")
                },
            )?;
            return Ok(Some(window));
        };
        self.load_legacy_latest_window(&pool, session_id, scope_key)
            .await
    }

    async fn persist_latest_window(
        &self,
        session_id: &str,
        scope_key: &str,
        window: &BridgeReplayWindow,
    ) -> Result<(), String> {
        let pool = self.pool.get().clone();
        let replay_window_json =
            serde_json::to_string(window).map_err(|error| error.to_string())?;
        query(
            "INSERT INTO agent_session_replay_scope_windows \
             (session_id, scope_key, replay_window_json, created_at, updated_at) \
             VALUES (?, ?, ?, NOW(6), NOW(6)) \
             ON DUPLICATE KEY UPDATE \
               replay_window_json = VALUES(replay_window_json), \
               updated_at = NOW(6)",
        )
        .bind(session_id)
        .bind(scope_key)
        .bind(replay_window_json)
        .execute(&pool)
        .await
        .map_err(|error| error.to_string())?;
        self.trim_scope_windows(&pool, session_id).await?;
        Ok(())
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct PersistedBridgeReplayWindowEntry {
    window: BridgeReplayWindow,
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
struct InMemoryBridgeReplayWindowStore {
    entries:
        Arc<tokio::sync::Mutex<HashMap<String, HashMap<String, PersistedBridgeReplayWindowEntry>>>>,
}

#[cfg(test)]
#[async_trait]
impl BridgeReplayWindowStore for InMemoryBridgeReplayWindowStore {
    async fn load_latest_window(
        &self,
        session_id: &str,
        scope_key: &str,
    ) -> Result<Option<BridgeReplayWindow>, String> {
        let entries = self.entries.lock().await;
        Ok(entries
            .get(session_id)
            .and_then(|scopes| scopes.get(scope_key))
            .map(|entry| entry.window.clone()))
    }

    async fn persist_latest_window(
        &self,
        session_id: &str,
        scope_key: &str,
        window: &BridgeReplayWindow,
    ) -> Result<(), String> {
        let mut entries = self.entries.lock().await;
        entries.entry(session_id.to_string()).or_default().insert(
            scope_key.to_string(),
            PersistedBridgeReplayWindowEntry {
                window: window.clone(),
            },
        );
        Ok(())
    }
}

fn replay_window_total_bytes(frames: &[String]) -> usize {
    frames.iter().map(|frame| frame.len()).sum()
}

fn replay_window_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn bridge_replay_window_key(
    session_id: &str,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) -> String {
    format!(
        "{BRIDGE_REPLAY_WINDOW_PREFIX}:{session_id}:{}:{}",
        turn_chain_id.unwrap_or_default(),
        user_query_event_id.unwrap_or_default()
    )
}

fn request_scope_cursor(
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) -> Option<StreamEventCursor> {
    Some(StreamEventCursor {
        sequence: 0,
        session_id: Some(session_id?.to_string()),
        turn_chain_id: turn_chain_id
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        user_query_event_id: user_query_event_id
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    })
}

async fn append_bridge_replay_frame(
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    persisted_replay_window_store: Option<Arc<dyn BridgeReplayWindowStore>>,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
    frame: &[u8],
) {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return;
    };
    let key = bridge_replay_window_key(session_id, turn_chain_id, user_query_event_id);
    let now = replay_window_now();
    let mut cache_guard = cache.lock().await;
    let entry = cache_guard.get(&key, now).unwrap_or_default();
    let mut window = BridgeReplayWindow::from_cache_entry(entry);
    if !window.append_frame(frame) {
        return;
    }
    let should_persist = window.should_persist();
    cache_guard.insert(key.clone(), window.clone().into_cache_entry(), now);
    drop(cache_guard);
    if should_persist && let Some(store) = persisted_replay_window_store {
        let mut persisted_window = window.clone();
        persisted_window.mark_persisted();
        if let Err(error) = store
            .persist_latest_window(session_id, &key, &persisted_window)
            .await
        {
            astra_core::agent_error!(
                "bridge",
                "persisted replay window update failed for session {}: {}",
                session_id,
                error
            );
        } else {
            let mut cache_guard = cache.lock().await;
            cache_guard.insert(
                key,
                persisted_window.into_cache_entry(),
                replay_window_now(),
            );
        }
    }
}

async fn replay_buffered_bridge_suffix(
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    resume_cursor: &StreamEventCursor,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
    allow_incomplete: bool,
) -> Option<BridgeReplaySuffix> {
    let request_scope = request_scope_cursor(session_id, turn_chain_id, user_query_event_id)?;
    if !resume_cursor.matches_scope(&request_scope) {
        return None;
    }
    let session_id = request_scope.session_id.as_deref()?;
    let key = bridge_replay_window_key(session_id, turn_chain_id, user_query_event_id);
    let now = replay_window_now();
    let mut cache = cache.lock().await;
    let entry = cache.get(&key, now)?;
    let window = BridgeReplayWindow::from_cache_entry(entry);
    window.suffix_after(resume_cursor, allow_incomplete)
}

async fn replay_persisted_bridge_suffix(
    persisted_replay_window_store: Option<Arc<dyn BridgeReplayWindowStore>>,
    resume_cursor: &StreamEventCursor,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
    allow_incomplete: bool,
) -> Option<BridgeReplaySuffix> {
    let request_scope = request_scope_cursor(session_id, turn_chain_id, user_query_event_id)?;
    if !resume_cursor.matches_scope(&request_scope) {
        return None;
    }
    let session_id = request_scope.session_id.as_deref()?;
    let key = bridge_replay_window_key(session_id, turn_chain_id, user_query_event_id);
    let store = persisted_replay_window_store?;
    let window = match store.load_latest_window(session_id, &key).await {
        Ok(window) => window?,
        Err(error) => {
            astra_core::agent_error!(
                "bridge",
                "persisted replay window load failed for session {}: {}",
                session_id,
                error
            );
            return None;
        }
    };
    window.suffix_after(resume_cursor, allow_incomplete)
}

async fn replay_bridge_suffix(
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    persisted_replay_window_store: Option<Arc<dyn BridgeReplayWindowStore>>,
    resume_cursor: Option<&StreamEventCursor>,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
    allow_incomplete: bool,
) -> Option<BridgeReplaySuffix> {
    let resume_cursor = resume_cursor?;
    if let Some(suffix) = replay_buffered_bridge_suffix(
        cache,
        resume_cursor,
        session_id,
        turn_chain_id,
        user_query_event_id,
        allow_incomplete,
    )
    .await
    {
        return Some(suffix);
    }
    replay_persisted_bridge_suffix(
        persisted_replay_window_store,
        resume_cursor,
        session_id,
        turn_chain_id,
        user_query_event_id,
        allow_incomplete,
    )
    .await
}

fn replay_window_response(frames: Vec<Bytes>) -> Response {
    let mut body = Vec::new();
    for frame in frames {
        body.extend_from_slice(&frame);
    }
    sse_stream_response(StatusCode::OK, Body::from(body))
}

fn last_event_id_cursor(headers: &HeaderMap) -> Option<StreamEventCursor> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_stream_event_id)
        .filter(StreamEventCursor::has_replay_scope)
}

fn forwarded_last_event_id(
    resume_cursor: Option<&StreamEventCursor>,
    replay_suffix: Option<&BridgeReplaySuffix>,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) -> Option<String> {
    let request_scope = request_scope_cursor(session_id, turn_chain_id, user_query_event_id)?;
    let resume_cursor = resume_cursor.filter(|cursor| cursor.matches_scope(&request_scope))?;
    let cursor = replay_suffix
        .map(|suffix| &suffix.last_cursor)
        .filter(|cursor| {
            resume_cursor.matches_scope(cursor) && cursor.sequence > resume_cursor.sequence
        })
        .unwrap_or(resume_cursor);
    Some(render_stream_event_id(
        cursor.sequence,
        cursor.session_id.as_deref(),
        cursor.turn_chain_id.as_deref(),
        cursor.user_query_event_id.as_deref(),
    ))
}

fn stream_cursor_from_frame(frame: &[u8]) -> Option<StreamEventCursor> {
    parse_sse_json_frame(frame).and_then(|event| StreamEventCursor::from_event(&event))
}

fn filter_replayed_stream_events<S>(
    stream: S,
    resume_cursor: Option<StreamEventCursor>,
) -> impl futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    stream! {
        let mut stream = Box::pin(stream);
        let mut resume_cursor = resume_cursor;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if let Some(cursor) = resume_cursor.as_ref()
                        && let Some(event_cursor) = stream_cursor_from_frame(&bytes)
                        && cursor.matches_scope(&event_cursor)
                    {
                        if event_cursor.sequence <= cursor.sequence {
                            continue;
                        }
                        resume_cursor = None;
                    }
                    yield Ok(bytes);
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
    }
}

fn record_bridge_replay_window_stream<S>(
    stream: S,
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    persisted_replay_window_store: Option<Arc<dyn BridgeReplayWindowStore>>,
    trusted_session_id: Option<String>,
    trusted_turn_chain_id: Option<String>,
    trusted_user_query_event_id: Option<String>,
) -> impl futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    stream! {
        let mut stream = Box::pin(stream);
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    append_bridge_replay_frame(
                        cache.clone(),
                        persisted_replay_window_store.clone(),
                        trusted_session_id.as_deref(),
                        trusted_turn_chain_id.as_deref(),
                        trusted_user_query_event_id.as_deref(),
                        &bytes,
                    )
                    .await;
                    yield Ok(bytes);
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
    }
}

struct BridgeResponseStream<S> {
    stream: Pin<Box<S>>,
    _disconnect_guard: Option<crate::turn::llm_client::CancelOnClientDisconnect>,
}

impl<S> BridgeResponseStream<S>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    fn new(stream: S, client_cancel: Option<Arc<CancellationToken>>) -> Self {
        Self {
            stream: Box::pin(stream),
            _disconnect_guard: client_cancel
                .map(crate::turn::llm_client::CancelOnClientDisconnect::new),
        }
    }
}

impl<S> futures_util::stream::Stream for BridgeResponseStream<S>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

fn is_bridge_sse_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| value.starts_with("text/event-stream"))
}

fn bridge_http_response_health(status: StatusCode, is_sse: bool) -> (bool, bool) {
    let is_success = status.is_success() && is_sse;
    let should_trip_breaker =
        status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || !is_sse;
    (is_success, should_trip_breaker)
}

fn should_replay_incomplete_suffix_for_pre_stream_failure(
    status: StatusCode,
    is_sse: bool,
) -> bool {
    !is_sse
        && (status.is_server_error()
            || status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::REQUEST_TIMEOUT
            || status.is_success())
}

fn bridge_error_sse_message(status: StatusCode, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!(
            "Bridge returned HTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("error")
        );
    }
    format!(
        "Bridge returned HTTP {} {}: {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("error"),
        trimmed
    )
}

fn bridge_status_to_sse_error_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "AUTH_ERROR",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::UNPROCESSABLE_ENTITY => "VALIDATION_ERROR",
        StatusCode::TOO_MANY_REQUESTS => "RATE_LIMIT",
        StatusCode::REQUEST_TIMEOUT
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => "UPSTREAM_ERROR",
        _ => "INTERNAL_ERROR",
    }
}

fn bridge_error_sse_response(
    status: StatusCode,
    message: impl Into<String>,
    trusted_session_id: Option<&str>,
    trusted_run_id: Option<&str>,
) -> Response {
    let event = serde_json::json!({
        "type": "error",
        "message": message.into(),
        "code": bridge_status_to_sse_error_code(status),
        "retryable": status.is_server_error()
            || matches!(
                status,
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::REQUEST_TIMEOUT
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ),
    });
    let mut body = Vec::new();
    let mut sequence = 0u64;
    if let Some(session_id) = trusted_session_id {
        body.extend_from_slice(&render_stream_event_bytes(
            synthesized_session_info_event(session_id, trusted_run_id),
            &mut sequence,
            Some(session_id),
            trusted_run_id,
            None,
        ));
    }
    body.extend_from_slice(&render_stream_event_bytes(
        event,
        &mut sequence,
        trusted_session_id,
        trusted_run_id,
        None,
    ));
    sse_stream_response(StatusCode::OK, Body::from(body))
}

fn trusted_bridge_identity(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let trusted_session_id = headers
        .get("x-mo-session-id")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        .filter(|value| !value.is_empty());
    let trusted_turn_chain_id = headers
        .get("x-mo-turn-chain-id")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        .filter(|value| !value.is_empty());
    (trusted_session_id, trusted_turn_chain_id)
}

async fn read_bridge_error_body_excerpt<S, E>(mut stream: S, max_bytes: usize) -> String
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Unpin,
{
    use futures_util::StreamExt;

    let mut collected = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = max_bytes.saturating_sub(collected.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        if chunk.len() > remaining {
            collected.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        collected.extend_from_slice(&chunk);
    }
    let mut text = String::from_utf8_lossy(&collected).into_owned();
    if truncated {
        text.push_str("\n… [truncated]");
    }
    text
}

fn build_max_rounds_turn_complete_event(
    bridge_state: &serde_json::Map<String, serde_json::Value>,
    trusted_execution_state: Option<&serde_json::Value>,
    latest_user_message: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut turn_complete = build_turn_complete_event_from_bridge_state(
        bridge_state,
        trusted_execution_state,
        latest_user_message,
    );
    turn_complete.insert(
        "max_rounds_exceeded".to_string(),
        serde_json::Value::Bool(true),
    );
    turn_complete
}

#[async_trait]
pub trait ChatTurnBridge: Send + Sync {
    /// Last argument: optional cancel token — HTTP `/chat/turn` passes one so dropping the SSE body
    /// (client disconnect) stops in-process LLM streaming promptly.
    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        headers: &HeaderMap,
        body: Bytes,
        turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        turn_observer_worker: Arc<dyn TurnObserverWorker>,
        turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        client_cancel: Option<Arc<CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)>;
}

#[derive(Clone)]
pub(crate) struct HttpChatTurnBridge {
    url: String,
    client: reqwest::Client,
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    circuit_breaker: Arc<CircuitBreaker>,
    health_metrics: Arc<BridgeHealthMetrics>,
    persisted_replay_window_store: Option<Arc<dyn BridgeReplayWindowStore>>,
    turn_learning_writer: Option<Arc<dyn TurnLearningWriter>>,
}

impl std::fmt::Debug for HttpChatTurnBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpChatTurnBridge")
            .field("url", &self.url)
            .field(
                "has_persisted_replay_window_store",
                &self.persisted_replay_window_store.is_some(),
            )
            .field("has_learning_writer", &self.turn_learning_writer.is_some())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UnavailableChatTurnBridge;

#[derive(Clone, Debug)]
struct BridgeSideEffectRequestContext {
    messages: Vec<Value>,
    tool_results: Vec<Value>,
    agent_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InMemoryTurnReflectionStateStore {
    pub(crate) state: Arc<tokio::sync::Mutex<HashMap<String, TurnReflectionMark>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnReflectionLessonWriter;

#[derive(Clone, Debug)]
pub struct DatabaseTurnReflectionLessonWriter {
    pub(crate) base_url: String,
    pub(crate) master_key: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnObserverWorker;

#[derive(Clone, Debug)]
pub struct DatabaseTurnObserverWorker {
    pub(crate) base_url: String,
    pub(crate) master_key: Option<String>,
}

impl HttpChatTurnBridge {
    pub(crate) fn new(url: String, cache: Arc<tokio::sync::Mutex<SessionCache>>) -> Self {
        Self {
            url,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("chat turn bridge client should build"),
            cache,
            circuit_breaker: Arc::new(CircuitBreaker::with_defaults()),
            health_metrics: Arc::new(BridgeHealthMetrics::new()),
            persisted_replay_window_store: None,
            turn_learning_writer: None,
        }
    }

    pub(crate) fn with_persisted_replay_window_store(
        mut self,
        store: Arc<dyn BridgeReplayWindowStore>,
    ) -> Self {
        self.persisted_replay_window_store = Some(store);
        self
    }

    /// Set the pipeline learning writer for this bridge.
    pub(crate) fn with_learning_writer(mut self, writer: Arc<dyn TurnLearningWriter>) -> Self {
        self.turn_learning_writer = Some(writer);
        self
    }

    /// Get a snapshot of bridge health metrics.
    #[cfg(test)]
    pub(crate) fn health_snapshot(&self) -> circuit_breaker::BridgeHealthSnapshot {
        self.health_metrics.snapshot()
    }
}

#[async_trait]
impl ChatTurnBridge for UnavailableChatTurnBridge {
    async fn forward(
        &self,
        headers: &HeaderMap,
        _body: Bytes,
        _turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        _turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        _turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        _turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        _turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        _turn_observer_worker: Arc<dyn TurnObserverWorker>,
        _turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        _turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        _client_cancel: Option<Arc<CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)> {
        let (trusted_session_id, trusted_turn_chain_id) = trusted_bridge_identity(headers);
        Ok(bridge_error_sse_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "chat turn bridge disabled. Configure CHAT_TURN_BRIDGE_URL to a reachable /internal/chat/turn endpoint (example: compatible chat-turn bridge service), then restart API."
                .to_string(),
            trusted_session_id.as_deref(),
            trusted_turn_chain_id.as_deref(),
        ))
    }
}

#[async_trait]
impl ChatTurnBridge for HttpChatTurnBridge {
    async fn forward(
        &self,
        headers: &HeaderMap,
        body: Bytes,
        turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        turn_observer_worker: Arc<dyn TurnObserverWorker>,
        turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        client_cancel: Option<Arc<CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)> {
        let mut bridge_headers = HeaderMap::new();
        let side_effect_request_context = parse_bridge_side_effect_request_context(&body);
        let resume_cursor = last_event_id_cursor(headers);
        let mut request = self
            .client
            .post(&self.url)
            .header("content-type", "application/json");
        for (header_name, value) in headers.iter() {
            if is_allowed_bridge_header(header_name.as_str())
                && let Ok(value_str) = value.to_str()
            {
                bridge_headers.insert(header_name.clone(), value.clone());
                request = request.header(header_name.as_str(), value_str);
            }
        }
        request = request.body(body.to_vec());
        let (trusted_session_id, trusted_turn_chain_id) = trusted_bridge_identity(&bridge_headers);
        let trusted_user_query_event_id = bridge_headers
            .get("x-mo-user-query-event-id")
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string)
            .filter(|value| !value.is_empty());

        let replay_suffix = replay_bridge_suffix(
            self.cache.clone(),
            self.persisted_replay_window_store.clone(),
            resume_cursor.as_ref(),
            trusted_session_id.as_deref(),
            trusted_turn_chain_id.as_deref(),
            trusted_user_query_event_id.as_deref(),
            true,
        )
        .await;
        if let Some(suffix) = replay_suffix.as_ref()
            && suffix.complete
        {
            return Ok(replay_window_response(suffix.frames.clone()));
        }
        let incomplete_replay_suffix = replay_suffix.filter(|suffix| !suffix.complete);
        if let Some(last_event_id) = forwarded_last_event_id(
            resume_cursor.as_ref(),
            incomplete_replay_suffix.as_ref(),
            trusted_session_id.as_deref(),
            trusted_turn_chain_id.as_deref(),
            trusted_user_query_event_id.as_deref(),
        ) {
            request = request.header("last-event-id", last_event_id);
        }

        // Circuit breaker: fast-reject if bridge is in open state
        if !self.circuit_breaker.allow_request() {
            if let Some(suffix) = incomplete_replay_suffix.clone() {
                return Ok(replay_window_response(suffix.frames));
            }
            let metrics = self.circuit_breaker.metrics();
            return Ok(bridge_error_sse_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Bridge circuit breaker is open (consecutive failures: {}, state: {}). Retry after recovery timeout.",
                    metrics.consecutive_failures, metrics.state
                ),
                trusted_session_id.as_deref(),
                trusted_turn_chain_id.as_deref(),
            ));
        }

        let request_start = std::time::Instant::now();
        let response = match request.send().await {
            Ok(resp) => resp,
            Err(error) => {
                let latency_ms = request_start.elapsed().as_millis() as u64;
                let is_timeout = error.is_timeout();
                self.circuit_breaker.record_failure();
                self.health_metrics
                    .record_request(latency_ms, false, is_timeout);
                if let Some(suffix) = incomplete_replay_suffix.clone() {
                    return Ok(replay_window_response(suffix.frames));
                }
                return Ok(bridge_error_sse_response(
                    StatusCode::BAD_GATEWAY,
                    error.to_string(),
                    trusted_session_id.as_deref(),
                    trusted_turn_chain_id.as_deref(),
                ));
            }
        };
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let is_sse = is_bridge_sse_content_type(content_type.as_deref());
        // These bridge headers were synthesized by server-side bridge prep before dispatch, so
        // they remain the authoritative session/run identity even if the upstream response is a
        // non-SSE error body with no usable metadata of its own.
        let request_latency_ms = request_start.elapsed().as_millis() as u64;
        let (is_success, should_trip_breaker) = bridge_http_response_health(status, is_sse);
        self.health_metrics
            .record_request(request_latency_ms, is_success, false);
        if is_success {
            self.circuit_breaker.record_success();
        } else if should_trip_breaker {
            self.circuit_breaker.record_failure();
        }
        if !is_sse {
            if let Some(suffix) = incomplete_replay_suffix
                .clone()
                .filter(|_| should_replay_incomplete_suffix_for_pre_stream_failure(status, is_sse))
            {
                return Ok(replay_window_response(suffix.frames));
            }
            let error_body = read_bridge_error_body_excerpt(
                response.bytes_stream(),
                MAX_BRIDGE_ERROR_BODY_BYTES,
            )
            .await;
            return Ok(bridge_error_sse_response(
                status,
                bridge_error_sse_message(status, &error_body),
                trusted_session_id.as_deref(),
                trusted_turn_chain_id.as_deref(),
            ));
        }
        let filtered_stream = filter_bridge_state_events(
            response.bytes_stream(),
            self.cache.clone(),
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                trusted_session_id.clone()
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                bridge_headers
                    .get("x-mo-user-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToString::to_string)
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                bridge_headers
                    .get("x-mo-execution-state-b64")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|encoded| URL_SAFE.decode(encoded).ok())
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                trusted_turn_chain_id.clone()
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                trusted_user_query_event_id.clone()
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                bridge_headers
                    .get("x-mo-routing-meta-b64")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|encoded| URL_SAFE.decode(encoded).ok())
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            } else {
                None
            },
            if status == StatusCode::OK
                && content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                side_effect_request_context
            } else {
                None
            },
            turn_core_event_writer,
            turn_tool_event_writer,
            turn_hook_db_writer,
            turn_reflection_state_store,
            turn_reflection_lesson_writer,
            turn_observer_worker,
            turn_auxiliary_event_writer,
            turn_session_activity_writer,
            self.turn_learning_writer.clone(),
        );
        let replay_prefix_frames = incomplete_replay_suffix
            .as_ref()
            .map(|suffix| suffix.frames.clone())
            .unwrap_or_default();
        let live_resume_cursor = incomplete_replay_suffix
            .as_ref()
            .map(|suffix| suffix.last_cursor.clone())
            .or(resume_cursor.clone());
        let live_stream = record_bridge_replay_window_stream(
            filter_replayed_stream_events(filtered_stream, live_resume_cursor),
            self.cache.clone(),
            self.persisted_replay_window_store.clone(),
            trusted_session_id.clone(),
            trusted_turn_chain_id.clone(),
            trusted_user_query_event_id,
        );
        let spliced_stream = futures_util::stream::iter(
            replay_prefix_frames
                .into_iter()
                .map(Ok::<Bytes, std::io::Error>),
        )
        .chain(live_stream);
        let response_stream = BridgeResponseStream::new(spliced_stream, client_cancel);
        Ok(sse_stream_response(
            StatusCode::OK,
            Body::from_stream(response_stream),
        ))
    }
}

fn parse_bridge_side_effect_request_context(
    body: &Bytes,
) -> Option<BridgeSideEffectRequestContext> {
    let payload = serde_json::from_slice::<Value>(body).ok()?;
    let object = payload.as_object()?;
    Some(BridgeSideEffectRequestContext {
        messages: object
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        tool_results: object
            .get("tool_results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        agent_id: object
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .filter(|value| !value.is_empty()),
    })
}

#[allow(clippy::too_many_arguments)]
fn filter_bridge_state_events<S>(
    mut stream: S,
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    trusted_session_id: Option<String>,
    side_effect_user_id: Option<String>,
    trusted_execution_state: Option<serde_json::Value>,
    trusted_turn_chain_id: Option<String>,
    trusted_user_query_event_id: Option<String>,
    trusted_routing_meta: Option<serde_json::Value>,
    side_effect_request_context: Option<BridgeSideEffectRequestContext>,
    turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
    turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
    turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
    turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
    turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
    turn_observer_worker: Arc<dyn TurnObserverWorker>,
    turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
    turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
    turn_learning_writer: Option<Arc<dyn TurnLearningWriter>>,
) -> impl futures_util::stream::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures_util::stream::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    fn latest_user_message_from_side_effect_inputs(
        side_effect_inputs: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Option<String> {
        side_effect_inputs
            .and_then(|inputs| inputs.get("messages"))
            .and_then(serde_json::Value::as_array)
            .and_then(|messages| {
                messages.iter().rev().find_map(|message| {
                    let object = message.as_object()?;
                    if object.get("role").and_then(serde_json::Value::as_str) != Some("user") {
                        return None;
                    }
                    object
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                })
            })
    }

    stream! {
        let mut event_sequence = 0u64;
        macro_rules! emit_event {
            ($event:expr) => {
                render_stream_event_bytes(
                    $event,
                    &mut event_sequence,
                    trusted_session_id.as_deref(),
                    trusted_turn_chain_id.as_deref(),
                    trusted_user_query_event_id.as_deref(),
                )
            };
        }
        macro_rules! emit_frame {
            ($frame:expr) => {
                rewrite_sse_frame_with_stream_metadata(
                    $frame,
                    &mut event_sequence,
                    trusted_session_id.as_deref(),
                    trusted_turn_chain_id.as_deref(),
                    trusted_user_query_event_id.as_deref(),
                )
            };
        }
        if let Some(session_id) = trusted_session_id.as_deref() {
            yield Ok(emit_event!(synthesized_session_info_event(
                session_id,
                trusted_turn_chain_id.as_deref(),
            )));
        }
        let mut buffer = Vec::new();
        let mut pending_bridge_state: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut pending_followup_user_message: Option<String> = None;
        let mut pending_warning_event: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut pending_explain_event: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut latest_token_usage: Option<serde_json::Value> = None;
        let mut suppress_next_turn_complete = false;
        let mut tool_rounds: i64 = 0;
        let mut received_turn_complete = false;
        let mut force_max_rounds_completion = false;
        let mut saw_error_event = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    buffer.extend_from_slice(&chunk);
                    if buffer.len() > MAX_SSE_BUFFER_BYTES {
                        yield Err(std::io::Error::other(format!(
                            "SSE buffer exceeded {} bytes — possible slow client or malformed stream",
                            MAX_SSE_BUFFER_BYTES
                        )));
                        return;
                    }
                    while let Some(end) = find_sse_frame_end(&buffer) {
                        let frame = buffer.drain(..end + 2).collect::<Vec<_>>();
                        if is_session_info_frame(&frame) {
                            if trusted_session_id.is_some() {
                                continue;
                            }
                            yield Ok(emit_frame!(&frame));
                            continue;
                        } else if suppress_next_turn_complete && is_turn_complete_frame(&frame) {
                            suppress_next_turn_complete = false;
                            continue;
                        } else if let Some(mut bridge_state) = parse_bridge_state_frame(&frame) {
                            let Some(trusted_session_id) = trusted_session_id.as_deref() else {
                                yield Ok(emit_frame!(&frame));
                                continue;
                            };
                            let prompt_fingerprints =
                                take_bridge_prompt_fingerprints(&mut bridge_state);
                            let tail_update_args =
                                take_bridge_tail_update_args(&mut bridge_state);
                            let warning_event = take_bridge_warning_event(&mut bridge_state);
                            let explain_event = take_bridge_explain_event(&mut bridge_state);
                            let side_effect_inputs =
                                take_bridge_side_effect_inputs(&mut bridge_state);
                            let followup_user_message =
                                latest_user_message_from_side_effect_inputs(side_effect_inputs.as_ref());
                            if let Some(response_guard_error) = tail_update_args
                                .as_ref()
                                .and_then(|tail_update_args| {
                                    build_bridge_response_guard_error_event(
                                        tail_update_args,
                                        &prompt_fingerprints,
                                    )
                                })
                            {
                                pending_bridge_state = None;
                                pending_followup_user_message = None;
                                pending_warning_event = None;
                                pending_explain_event = None;
                                if let Some(side_effect_inputs) = side_effect_inputs.as_ref()
                                    && let Some((persist_payload, hook_payload)) =
                                        build_bridge_response_guard_side_effect_payloads(
                                            side_effect_user_id.as_deref(),
                                            trusted_session_id,
                                            &bridge_state,
                                            side_effect_inputs,
                                            tail_update_args.as_ref(),
                                            trusted_turn_chain_id.as_deref(),
                                            trusted_user_query_event_id.as_deref(),
                                            latest_token_usage.as_ref(),
                                            trusted_routing_meta.as_ref(),
                                            side_effect_request_context.as_ref(),
                                        )
                                {
                                    dispatch_bridge_side_effect_request(
                                        Some(persist_payload),
                                        turn_core_event_writer.clone(),
                                        turn_tool_event_writer.clone(),
                                        turn_auxiliary_event_writer.clone(),
                                        turn_session_activity_writer.clone(),
                                    );
                                    run_bridge_hook_side_effects(
                                        Some(hook_payload),
                                        turn_hook_db_writer.clone(),
                                        turn_reflection_state_store.clone(),
                                        turn_reflection_lesson_writer.clone(),
                                        turn_observer_worker.clone(),
                                        turn_learning_writer.clone(),
                                    );
                                }
                                suppress_next_turn_complete = true;
                                if let Some(warning_event) = warning_event {
                                    yield Ok(emit_event!(serde_json::Value::Object(warning_event)));
                                }
                                if let Some(explain_event) = explain_event {
                                    yield Ok(emit_event!(serde_json::Value::Object(explain_event)));
                                }
                                yield Ok(emit_event!(serde_json::Value::Object(
                                    response_guard_error,
                                )));
                                yield Ok(emit_event!(serde_json::Value::Object(
                                    build_turn_complete_event_from_bridge_state(
                                        &bridge_state,
                                        trusted_execution_state.as_ref(),
                                        followup_user_message.as_deref(),
                                    ),
                                )));
                                received_turn_complete = true;
                                continue;
                            }
                            let synced_bridge_state =
                                sync_bridge_state_event(
                                    cache.clone(),
                                    trusted_session_id,
                                    bridge_state,
                                    tail_update_args.as_ref(),
                                    trusted_turn_chain_id.as_deref(),
                                    trusted_user_query_event_id.as_deref(),
                                )
                                .await;
                            if let Some(side_effect_inputs) = side_effect_inputs.as_ref()
                                && let Some((persist_payload, hook_payload)) =
                                    build_bridge_side_effect_payloads(
                                        side_effect_user_id.as_deref(),
                                        trusted_session_id,
                                        &synced_bridge_state,
                                        side_effect_inputs,
                                        tail_update_args.as_ref(),
                                        latest_token_usage.as_ref(),
                                        trusted_routing_meta.as_ref(),
                                        side_effect_request_context.as_ref(),
                                    )
                                {
                                    dispatch_bridge_side_effect_request(
                                        Some(persist_payload),
                                        turn_core_event_writer.clone(),
                                        turn_tool_event_writer.clone(),
                                        turn_auxiliary_event_writer.clone(),
                                        turn_session_activity_writer.clone(),
                                    );
                                    run_bridge_hook_side_effects(
                                        Some(hook_payload),
                                        turn_hook_db_writer.clone(),
                                        turn_reflection_state_store.clone(),
                                        turn_reflection_lesson_writer.clone(),
                                        turn_observer_worker.clone(),
                                        turn_learning_writer.clone(),
                                    );
                                }
                            pending_bridge_state = Some(synced_bridge_state);
                            pending_followup_user_message = followup_user_message;
                            pending_warning_event = warning_event;
                            pending_explain_event = explain_event;

                            // Track tool rounds from bridge_state frames
                            if let Some(sigs) = pending_bridge_state.as_ref().and_then(bridge_state_tool_signatures)
                                && !sigs.is_empty() {
                                    tool_rounds += 1;
                                    if tool_rounds > max_tool_rounds() {
                                        astra_core::agent_warn!("bridge", "Turn exceeded max_tool_rounds ({}), forcing completion", max_tool_rounds());
                                        if let Some(warning_event) = pending_warning_event.take() {
                                            yield Ok(emit_event!(serde_json::Value::Object(warning_event)));
                                        }
                                        if let Some(explain_event) = pending_explain_event.take() {
                                            yield Ok(emit_event!(serde_json::Value::Object(explain_event)));
                                        }
                                        let bridge_state = pending_bridge_state
                                            .as_ref()
                                            .expect("pending bridge state should exist");
                                        yield Ok(emit_event!(serde_json::Value::Object(
                                            build_max_rounds_turn_complete_event(
                                                bridge_state,
                                                trusted_execution_state.as_ref(),
                                                pending_followup_user_message.as_deref(),
                                            ),
                                        )));
                                        return;
                                     }
                                 }
                         } else if pending_bridge_state.is_some() {
                             if let Some(text_delta_event) = build_text_delta_event_from_frame(&frame) {
                                yield Ok(emit_event!(serde_json::Value::Object(text_delta_event)));
                            } else if let Some(reasoning_delta_event) =
                                build_reasoning_delta_event_from_frame(&frame)
                            {
                                yield Ok(emit_event!(serde_json::Value::Object(reasoning_delta_event)));
                            } else if let Some(usage_event) = build_usage_event_from_frame(&frame) {
                                latest_token_usage = build_token_usage_from_usage_event(&usage_event);
                                yield Ok(emit_event!(serde_json::Value::Object(usage_event)));
                            } else if let Some(tool_result_quality_event) =
                                build_tool_result_quality_event_from_frame(&frame)
                            {
                                yield Ok(emit_event!(serde_json::Value::Object(tool_result_quality_event)));
                            } else if let Some(cloud_loop_progress_event) =
                                build_cloud_loop_progress_event_from_frame(&frame)
                            {
                                yield Ok(emit_event!(serde_json::Value::Object(cloud_loop_progress_event)));
                            } else if let Some(cloud_tool_result_event) =
                                build_cloud_tool_result_event_from_frame(&frame)
                            {
                                yield Ok(emit_event!(serde_json::Value::Object(cloud_tool_result_event)));
                            } else if let Some(error_event) = build_error_event_from_frame(&frame) {
                                saw_error_event = true;
                                yield Ok(emit_event!(serde_json::Value::Object(error_event)));
                            } else if let Some(tool_call_event) = build_tool_call_event_from_frame(&frame) {
                                yield Ok(emit_event!(serde_json::Value::Object(tool_call_event)));
                            } else if let Some(tool_call_start_event) =
                                build_tool_call_start_event_from_frame(&frame)
                            {
                                yield Ok(emit_event!(serde_json::Value::Object(tool_call_start_event)));
                            } else if is_warning_frame(&frame) {
                                if pending_warning_event.is_none() {
                                    yield Ok(emit_frame!(&frame));
                                }
                                continue;
                            } else if is_explain_frame(&frame) {
                                if pending_explain_event.is_none() {
                                    yield Ok(emit_frame!(&frame));
                                }
                                continue;
                            } else if is_turn_complete_frame(&frame) {
                                if let Some(warning_event) = pending_warning_event.take() {
                                    yield Ok(emit_event!(serde_json::Value::Object(warning_event)));
                                }
                                if let Some(explain_event) = pending_explain_event.take() {
                                    yield Ok(emit_event!(serde_json::Value::Object(explain_event)));
                                }
                                let bridge_state = pending_bridge_state
                                    .as_ref()
                                    .expect("pending bridge state should exist");
                                yield Ok(emit_event!(serde_json::Value::Object(
                                    build_turn_complete_event_from_bridge_state(
                                        bridge_state,
                                        trusted_execution_state.as_ref(),
                                        pending_followup_user_message.as_deref(),
                                    ),
                                )));
                                received_turn_complete = true;
                                pending_bridge_state = None;
                                pending_followup_user_message = None;
                                pending_warning_event = None;
                                pending_explain_event = None;
                                latest_token_usage = None;
                            } else {
                                yield Ok(emit_frame!(&frame));
                            }
                        } else if let Some(text_delta_event) = build_text_delta_event_from_frame(&frame) {
                            yield Ok(emit_event!(serde_json::Value::Object(text_delta_event)));
                        } else if let Some(reasoning_delta_event) =
                            build_reasoning_delta_event_from_frame(&frame)
                        {
                            yield Ok(emit_event!(serde_json::Value::Object(reasoning_delta_event)));
                        } else if let Some(usage_event) = build_usage_event_from_frame(&frame) {
                            latest_token_usage = build_token_usage_from_usage_event(&usage_event);
                            yield Ok(emit_event!(serde_json::Value::Object(usage_event)));
                        } else if let Some(tool_result_quality_event) =
                            build_tool_result_quality_event_from_frame(&frame)
                        {
                            yield Ok(emit_event!(serde_json::Value::Object(tool_result_quality_event)));
                        } else if let Some(cloud_loop_progress_event) =
                            build_cloud_loop_progress_event_from_frame(&frame)
                        {
                            yield Ok(emit_event!(serde_json::Value::Object(cloud_loop_progress_event)));
                        } else if let Some(cloud_tool_result_event) =
                            build_cloud_tool_result_event_from_frame(&frame)
                        {
                            yield Ok(emit_event!(serde_json::Value::Object(cloud_tool_result_event)));
                        } else if let Some(error_event) = build_error_event_from_frame(&frame) {
                            saw_error_event = true;
                            yield Ok(emit_event!(serde_json::Value::Object(error_event)));
                        } else if let Some(tool_call_event) = build_tool_call_event_from_frame(&frame) {
                            yield Ok(emit_event!(serde_json::Value::Object(tool_call_event)));
                        } else if let Some(tool_call_start_event) =
                            build_tool_call_start_event_from_frame(&frame)
                        {
                            yield Ok(emit_event!(serde_json::Value::Object(tool_call_start_event)));
                        } else if is_turn_complete_frame(&frame) {
                            yield Ok(emit_frame!(&frame));
                            received_turn_complete = true;
                            continue;
                        } else if is_warning_frame(&frame) || is_explain_frame(&frame) {
                            yield Ok(emit_frame!(&frame));
                            continue;
                        } else {
                            yield Ok(emit_frame!(&frame));
                        }
                    }
                }
                Err(error) => {
                    if let Some(warning_event) = pending_warning_event.take() {
                        yield Ok(emit_event!(serde_json::Value::Object(warning_event)));
                    }
                    if let Some(explain_event) = pending_explain_event.take() {
                        yield Ok(emit_event!(serde_json::Value::Object(explain_event)));
                    }
                    yield Ok(emit_event!(serde_json::Value::Object(
                        build_stream_error_event(
                            &format!("Failed to read bridge response: {error}"),
                            "UPSTREAM_ERROR",
                            true,
                        ),
                    )));
                    if let Some(bridge_state) = pending_bridge_state.take() {
                        yield Ok(emit_event!(serde_json::Value::Object(
                            if force_max_rounds_completion {
                                build_max_rounds_turn_complete_event(
                                    &bridge_state,
                                    trusted_execution_state.as_ref(),
                                    pending_followup_user_message.as_deref(),
                                )
                            } else {
                                build_turn_complete_event_from_bridge_state(
                                    &bridge_state,
                                    trusted_execution_state.as_ref(),
                                    pending_followup_user_message.as_deref(),
                                )
                            },
                        )));
                    }
                    return;
                }
            }
        }

        if !buffer.is_empty() {
            if let Some(mut bridge_state) = parse_bridge_state_frame(&buffer) {
                let Some(trusted_session_id) = trusted_session_id.as_deref() else {
                    yield Ok(emit_frame!(&buffer));
                    return;
                };
                let prompt_fingerprints = take_bridge_prompt_fingerprints(&mut bridge_state);
                let tail_update_args = take_bridge_tail_update_args(&mut bridge_state);
                let warning_event = take_bridge_warning_event(&mut bridge_state);
                let explain_event = take_bridge_explain_event(&mut bridge_state);
                let side_effect_inputs = take_bridge_side_effect_inputs(&mut bridge_state);
                let followup_user_message =
                    latest_user_message_from_side_effect_inputs(side_effect_inputs.as_ref());
                if let Some(response_guard_error) = tail_update_args
                    .as_ref()
                    .and_then(|tail_update_args| {
                        build_bridge_response_guard_error_event(
                            tail_update_args,
                            &prompt_fingerprints,
                        )
                    })
                {
                    if let Some(side_effect_inputs) = side_effect_inputs.as_ref()
                        && let Some((persist_payload, hook_payload)) =
                            build_bridge_response_guard_side_effect_payloads(
                                side_effect_user_id.as_deref(),
                                trusted_session_id,
                                &bridge_state,
                                side_effect_inputs,
                                tail_update_args.as_ref(),
                                trusted_turn_chain_id.as_deref(),
                                trusted_user_query_event_id.as_deref(),
                                latest_token_usage.as_ref(),
                                trusted_routing_meta.as_ref(),
                                side_effect_request_context.as_ref(),
                            )
                    {
                        dispatch_bridge_side_effect_request(
                            Some(persist_payload),
                            turn_core_event_writer.clone(),
                            turn_tool_event_writer.clone(),
                            turn_auxiliary_event_writer.clone(),
                            turn_session_activity_writer.clone(),
                        );
                        run_bridge_hook_side_effects(
                            Some(hook_payload),
                            turn_hook_db_writer.clone(),
                            turn_reflection_state_store.clone(),
                            turn_reflection_lesson_writer.clone(),
                            turn_observer_worker.clone(),
                            turn_learning_writer.clone(),
                        );
                    }
                    if let Some(warning_event) = warning_event {
                        yield Ok(emit_event!(serde_json::Value::Object(warning_event)));
                    }
                    if let Some(explain_event) = explain_event {
                        yield Ok(emit_event!(serde_json::Value::Object(explain_event)));
                    }
                    yield Ok(emit_event!(serde_json::Value::Object(response_guard_error)));
                    yield Ok(emit_event!(serde_json::Value::Object(
                        build_turn_complete_event_from_bridge_state(
                            &bridge_state,
                            trusted_execution_state.as_ref(),
                            followup_user_message.as_deref(),
                        ),
                    )));
                    received_turn_complete = true;
                } else {
                    let synced_bridge_state =
                        sync_bridge_state_event(
                            cache,
                            trusted_session_id,
                            bridge_state,
                            tail_update_args.as_ref(),
                            trusted_turn_chain_id.as_deref(),
                            trusted_user_query_event_id.as_deref(),
                        )
                        .await;
                    if let Some(side_effect_inputs) = side_effect_inputs.as_ref()
                        && let Some((persist_payload, hook_payload)) =
                            build_bridge_side_effect_payloads(
                                side_effect_user_id.as_deref(),
                                trusted_session_id,
                                &synced_bridge_state,
                                side_effect_inputs,
                                tail_update_args.as_ref(),
                                latest_token_usage.as_ref(),
                                trusted_routing_meta.as_ref(),
                                side_effect_request_context.as_ref(),
                            )
                        {
                            dispatch_bridge_side_effect_request(
                                Some(persist_payload),
                                turn_core_event_writer.clone(),
                                turn_tool_event_writer.clone(),
                                turn_auxiliary_event_writer.clone(),
                                turn_session_activity_writer.clone(),
                            );
                            run_bridge_hook_side_effects(
                                Some(hook_payload),
                                turn_hook_db_writer.clone(),
                                turn_reflection_state_store.clone(),
                                turn_reflection_lesson_writer.clone(),
                                turn_observer_worker.clone(),
                                turn_learning_writer.clone(),
                            );
                        }
                    pending_bridge_state = Some(synced_bridge_state);
                    pending_followup_user_message = followup_user_message;
                    pending_warning_event = warning_event;
                    pending_explain_event = explain_event;
                    if let Some(sigs) =
                        pending_bridge_state.as_ref().and_then(bridge_state_tool_signatures)
                        && !sigs.is_empty()
                    {
                        tool_rounds += 1;
                        if tool_rounds > max_tool_rounds() {
                            astra_core::agent_warn!(
                                "bridge",
                                "Turn exceeded max_tool_rounds ({}) in buffered tail frame, forcing completion",
                                max_tool_rounds()
                            );
                            force_max_rounds_completion = true;
                        }
                    }
                }
            } else {
                if is_session_info_frame(&buffer) {
                    if trusted_session_id.is_none() {
                        yield Ok(emit_frame!(&buffer));
                    }
                } else if suppress_next_turn_complete && is_turn_complete_frame(&buffer) {
                } else if pending_bridge_state.is_some() {
                    if let Some(text_delta_event) = build_text_delta_event_from_frame(&buffer) {
                        yield Ok(emit_event!(serde_json::Value::Object(text_delta_event)));
                    } else if let Some(reasoning_delta_event) =
                        build_reasoning_delta_event_from_frame(&buffer)
                    {
                        yield Ok(emit_event!(serde_json::Value::Object(reasoning_delta_event)));
                    } else if let Some(usage_event) = build_usage_event_from_frame(&buffer) {
                        yield Ok(emit_event!(serde_json::Value::Object(usage_event)));
                    } else if let Some(tool_result_quality_event) =
                        build_tool_result_quality_event_from_frame(&buffer)
                    {
                        yield Ok(emit_event!(serde_json::Value::Object(tool_result_quality_event)));
                    } else if let Some(cloud_loop_progress_event) =
                        build_cloud_loop_progress_event_from_frame(&buffer)
                    {
                        yield Ok(emit_event!(serde_json::Value::Object(cloud_loop_progress_event)));
                    } else if let Some(cloud_tool_result_event) =
                        build_cloud_tool_result_event_from_frame(&buffer)
                    {
                        yield Ok(emit_event!(serde_json::Value::Object(cloud_tool_result_event)));
                    } else if let Some(error_event) = build_error_event_from_frame(&buffer) {
                        saw_error_event = true;
                        yield Ok(emit_event!(serde_json::Value::Object(error_event)));
                    } else if let Some(tool_call_event) = build_tool_call_event_from_frame(&buffer) {
                        yield Ok(emit_event!(serde_json::Value::Object(tool_call_event)));
                    } else if let Some(tool_call_start_event) =
                        build_tool_call_start_event_from_frame(&buffer)
                    {
                        yield Ok(emit_event!(serde_json::Value::Object(tool_call_start_event)));
                    } else if is_warning_frame(&buffer) {
                        if pending_warning_event.is_none() {
                            yield Ok(emit_frame!(&buffer));
                        }
                    } else if is_explain_frame(&buffer) {
                        if pending_explain_event.is_none() {
                            yield Ok(emit_frame!(&buffer));
                        }
                    } else if is_turn_complete_frame(&buffer) {
                        if let Some(warning_event) = pending_warning_event.take() {
                            yield Ok(emit_event!(serde_json::Value::Object(warning_event)));
                        }
                        if let Some(explain_event) = pending_explain_event.take() {
                            yield Ok(emit_event!(serde_json::Value::Object(explain_event)));
                        }
                        let bridge_state = pending_bridge_state
                            .as_ref()
                            .expect("pending bridge state should exist");
                        yield Ok(emit_event!(serde_json::Value::Object(
                            build_turn_complete_event_from_bridge_state(
                                bridge_state,
                                trusted_execution_state.as_ref(),
                                pending_followup_user_message.as_deref(),
                            ),
                        )));
                        received_turn_complete = true;
                        pending_bridge_state = None;
                        pending_followup_user_message = None;
                        pending_warning_event = None;
                        pending_explain_event = None;
                    } else {
                        yield Ok(emit_frame!(&buffer));
                    }
                } else if let Some(text_delta_event) = build_text_delta_event_from_frame(&buffer) {
                    yield Ok(emit_event!(serde_json::Value::Object(text_delta_event)));
                } else if let Some(reasoning_delta_event) =
                    build_reasoning_delta_event_from_frame(&buffer)
                {
                    yield Ok(emit_event!(serde_json::Value::Object(reasoning_delta_event)));
                } else if let Some(usage_event) = build_usage_event_from_frame(&buffer) {
                    yield Ok(emit_event!(serde_json::Value::Object(usage_event)));
                } else if let Some(tool_result_quality_event) =
                    build_tool_result_quality_event_from_frame(&buffer)
                {
                    yield Ok(emit_event!(serde_json::Value::Object(tool_result_quality_event)));
                } else if let Some(cloud_loop_progress_event) =
                    build_cloud_loop_progress_event_from_frame(&buffer)
                {
                    yield Ok(emit_event!(serde_json::Value::Object(cloud_loop_progress_event)));
                } else if let Some(cloud_tool_result_event) =
                    build_cloud_tool_result_event_from_frame(&buffer)
                {
                    yield Ok(emit_event!(serde_json::Value::Object(cloud_tool_result_event)));
                } else if let Some(error_event) = build_error_event_from_frame(&buffer) {
                    saw_error_event = true;
                    yield Ok(emit_event!(serde_json::Value::Object(error_event)));
                } else if let Some(tool_call_event) = build_tool_call_event_from_frame(&buffer) {
                    yield Ok(emit_event!(serde_json::Value::Object(tool_call_event)));
                } else if let Some(tool_call_start_event) =
                    build_tool_call_start_event_from_frame(&buffer)
                {
                    yield Ok(emit_event!(serde_json::Value::Object(tool_call_start_event)));
                } else if is_turn_complete_frame(&buffer) {
                    yield Ok(emit_frame!(&buffer));
                    received_turn_complete = true;
                } else if is_warning_frame(&buffer) || is_explain_frame(&buffer) {
                    yield Ok(emit_frame!(&buffer));
                } else {
                    yield Ok(emit_frame!(&buffer));
                }
            }
        }
        if let Some(bridge_state) = pending_bridge_state {
            if let Some(warning_event) = pending_warning_event.take() {
                yield Ok(emit_event!(serde_json::Value::Object(warning_event)));
            }
            if let Some(explain_event) = pending_explain_event.take() {
                yield Ok(emit_event!(serde_json::Value::Object(explain_event)));
            }
            yield Ok(emit_event!(serde_json::Value::Object(
                if force_max_rounds_completion {
                    build_max_rounds_turn_complete_event(
                        &bridge_state,
                        trusted_execution_state.as_ref(),
                        pending_followup_user_message.as_deref(),
                    )
                } else {
                    build_turn_complete_event_from_bridge_state(
                        &bridge_state,
                        trusted_execution_state.as_ref(),
                        pending_followup_user_message.as_deref(),
                    )
                },
            )));
            received_turn_complete = true;
        }

        if !received_turn_complete {
            if !saw_error_event {
                yield Ok(emit_event!(serde_json::Value::Object(
                    build_stream_error_event(
                        "Bridge stream ended before turn_complete",
                        "UPSTREAM_ERROR",
                        true,
                    ),
                )));
            }
            astra_core::agent_warn!("bridge", "SSE stream ended without turn_complete frame — possible interruption");
        }
    }
}

pub(crate) fn sse_stream_response(status: StatusCode, body: Body) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    // ── Phase 6.6: Header filtering security tests ──

    #[test]
    fn allows_x_mo_headers() {
        assert!(is_allowed_bridge_header("x-mo-session-id"));
        assert!(is_allowed_bridge_header("x-mo-user-id"));
        assert!(is_allowed_bridge_header("x-mo-routing-meta-b64"));
    }

    #[test]
    fn allows_authorization() {
        assert!(is_allowed_bridge_header("authorization"));
    }

    #[test]
    fn blocks_dangerous_headers() {
        assert!(!is_allowed_bridge_header("cookie"));
        assert!(!is_allowed_bridge_header("set-cookie"));
        assert!(!is_allowed_bridge_header("host"));
        assert!(!is_allowed_bridge_header("x-forwarded-for"));
        assert!(!is_allowed_bridge_header("x-real-ip"));
        assert!(!is_allowed_bridge_header("origin"));
        assert!(!is_allowed_bridge_header("referer"));
    }

    #[test]
    fn blocks_content_type_override() {
        assert!(!is_allowed_bridge_header("content-type"));
    }

    #[test]
    fn blocks_prefix_spoof() {
        // "x-mobile" starts with "x-mo" but not "x-mo-"
        assert!(!is_allowed_bridge_header("x-mobile"));
        assert!(is_allowed_bridge_header("x-mo-"));
    }

    // ── BridgeHealthMetrics wiring test ──

    #[test]
    fn http_bridge_has_health_metrics() {
        use tokio::sync::Mutex;
        let cache = Arc::new(Mutex::new(SessionCache::default()));
        let bridge = HttpChatTurnBridge::new("http://localhost:9999".to_string(), cache);
        let snap = bridge.health_snapshot();
        assert_eq!(snap.total_requests, 0);
        assert_eq!(snap.total_failures, 0);
        assert_eq!(snap.failure_rate, 0.0);
    }

    #[test]
    fn synthesized_session_info_includes_trusted_run_id() {
        let event = synthesized_session_info_event("sess-1", Some("run-1"));
        assert_eq!(event["type"], "session_info");
        assert_eq!(event["session_id"], "sess-1");
        assert_eq!(event["run_id"], "run-1");
    }

    #[test]
    fn render_stream_event_bytes_adds_monotonic_sequence_and_correlation() {
        let mut sequence = 0u64;
        let first = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "a"}),
            &mut sequence,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        let second = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "b"}),
            &mut sequence,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        let first_text = String::from_utf8(first.to_vec()).expect("first frame should be utf8");
        let first = parse_sse_json_frame(&first).expect("first frame should parse");
        let second = parse_sse_json_frame(&second).expect("second frame should parse");
        assert!(first_text.starts_with("id: mo-stream-v1."));
        assert_eq!(first["sequence"], 1);
        assert_eq!(second["sequence"], 2);
        assert_eq!(first["turn_chain_id"], "turn-1");
        assert_eq!(first["user_query_event_id"], "query-1");
        assert_eq!(second["turn_chain_id"], "turn-1");
        assert_eq!(second["user_query_event_id"], "query-1");
        assert!(first["event_id"].as_str().is_some());
        assert!(second["event_id"].as_str().is_some());
    }

    #[test]
    fn rewrite_sse_frame_with_stream_metadata_rewrites_raw_json_frames() {
        let mut sequence = 0u64;
        let frame = b"data: {\"type\":\"warning\",\"message\":\"watch out\"}\n\n";
        let rewritten = rewrite_sse_frame_with_stream_metadata(
            frame,
            &mut sequence,
            Some("sess-2"),
            Some("turn-2"),
            Some("query-2"),
        );
        let parsed = parse_sse_json_frame(&rewritten).expect("rewritten frame should parse");
        assert_eq!(parsed["type"], "warning");
        assert_eq!(parsed["message"], "watch out");
        assert_eq!(parsed["sequence"], 1);
        assert_eq!(parsed["turn_chain_id"], "turn-2");
        assert_eq!(parsed["user_query_event_id"], "query-2");
        assert!(parsed["event_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn replay_wrapper_skips_replayed_prefix_when_last_event_matches_scope() {
        use futures_util::stream;

        let mut sequence = 0u64;
        let first = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "a"}),
            &mut sequence,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        let second = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "b"}),
            &mut sequence,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        let third = render_stream_event_bytes(
            serde_json::json!({"type": "turn_complete"}),
            &mut sequence,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        let resume_cursor = stream_cursor_from_frame(&second).expect("resume cursor should parse");
        let filtered = filter_replayed_stream_events(
            stream::iter(vec![
                Ok::<Bytes, std::io::Error>(first),
                Ok::<Bytes, std::io::Error>(second),
                Ok::<Bytes, std::io::Error>(third),
            ]),
            Some(resume_cursor),
        );
        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(!text.contains("\"content\":\"a\""));
        assert!(!text.contains("\"content\":\"b\""));
        assert!(text.contains("\"type\":\"turn_complete\""));
    }

    #[test]
    fn replay_window_requires_contiguous_suffix_coverage() {
        let mut sequence = 0u64;
        let first = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "a"}),
            &mut sequence,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        let second = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "b"}),
            &mut sequence,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        let third = render_stream_event_bytes(
            serde_json::json!({"type": "turn_complete"}),
            &mut sequence,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        let mut window = BridgeReplayWindow::default();
        assert!(window.append_frame(&second));
        assert!(window.append_frame(&third));
        let mut first_cursor = stream_cursor_from_frame(&first).expect("first cursor");
        let second_cursor = stream_cursor_from_frame(&second).expect("second cursor");
        first_cursor.sequence = 0;
        assert!(window.suffix_after(&first_cursor, false).is_none());
        let first = stream_cursor_from_frame(&first).expect("first cursor should still parse");
        let full_suffix = window
            .suffix_after(&first, false)
            .expect("window should replay suffix after first");
        let full_text = String::from_utf8(full_suffix.frames.concat().to_vec()).expect("utf8");
        assert!(full_text.contains("\"content\":\"b\""));
        let suffix = window
            .suffix_after(&second_cursor, false)
            .expect("window should replay suffix after second");
        let text = String::from_utf8(suffix.frames.concat().to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"turn_complete\""));
    }

    fn trusted_resume_headers(last_event_id: &str) -> HeaderMap {
        let mut headers = trusted_identity_headers();
        headers.insert(
            axum::http::header::HeaderName::from_static("last-event-id"),
            HeaderValue::from_str(last_event_id).expect("last-event-id should be valid"),
        );
        headers
    }

    fn trusted_identity_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-session-id", HeaderValue::from_static("sess-1"));
        headers.insert("x-mo-turn-chain-id", HeaderValue::from_static("run-1"));
        headers
    }

    async fn forward_with_noop_writers<B: ChatTurnBridge + ?Sized>(
        bridge: &B,
        headers: &HeaderMap,
    ) -> Result<Response, (StatusCode, String)> {
        bridge
            .forward(
                headers,
                Bytes::from_static(b"{}"),
                Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
                Arc::new(crate::turn::services::NoopTurnToolEventWriter),
                Arc::new(crate::turn::services::NoopTurnHookDbWriter),
                Arc::new(InMemoryTurnReflectionStateStore::default()),
                Arc::new(NoopTurnReflectionLessonWriter),
                Arc::new(NoopTurnObserverWorker),
                Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
                Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
                None,
            )
            .await
    }

    async fn response_text(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        String::from_utf8(body.to_vec()).expect("utf8")
    }

    #[test]
    fn bridge_http_500_counts_as_failure_and_breaker_signal() {
        let (is_success, should_trip_breaker) =
            bridge_http_response_health(StatusCode::INTERNAL_SERVER_ERROR, true);
        assert!(!is_success);
        assert!(should_trip_breaker);
    }

    #[test]
    fn bridge_non_sse_200_counts_as_failure_and_breaker_signal() {
        let (is_success, should_trip_breaker) = bridge_http_response_health(StatusCode::OK, false);
        assert!(!is_success);
        assert!(should_trip_breaker);
    }

    #[tokio::test]
    async fn circuit_breaker_fast_reject_preserves_trusted_session_info() {
        use tokio::sync::Mutex;

        let cache = Arc::new(Mutex::new(SessionCache::default()));
        let bridge = HttpChatTurnBridge::new("http://localhost:9999".to_string(), cache);
        for _ in 0..5 {
            bridge.circuit_breaker.record_failure();
        }

        let response = forward_with_noop_writers(&bridge, &trusted_identity_headers())
            .await
            .expect("fast reject should normalize to SSE");
        let text = response_text(response).await;

        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
    }

    #[tokio::test]
    async fn request_send_failure_preserves_trusted_session_info() {
        use tokio::sync::Mutex;

        let cache = Arc::new(Mutex::new(SessionCache::default()));
        let bridge = HttpChatTurnBridge::new("http://[::1".to_string(), cache);

        let response = forward_with_noop_writers(&bridge, &trusted_identity_headers())
            .await
            .expect("send failure should normalize to SSE");
        let text = response_text(response).await;

        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
    }

    #[tokio::test]
    async fn request_send_failure_replays_cached_suffix_before_touching_upstream() {
        use tokio::sync::Mutex;

        let cache = Arc::new(Mutex::new(SessionCache::new(1000, 86400.0)));
        let bridge = HttpChatTurnBridge::new("http://[::1".to_string(), cache.clone());
        let mut sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let turn_complete = render_stream_event_bytes(
            serde_json::json!({"type": "turn_complete"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        append_bridge_replay_frame(
            cache.clone(),
            None,
            Some("sess-1"),
            Some("run-1"),
            None,
            &session_info,
        )
        .await;
        append_bridge_replay_frame(
            cache.clone(),
            None,
            Some("sess-1"),
            Some("run-1"),
            None,
            &text_delta,
        )
        .await;
        append_bridge_replay_frame(
            cache.clone(),
            None,
            Some("sess-1"),
            Some("run-1"),
            None,
            &turn_complete,
        )
        .await;
        let last_event_id = parse_sse_json_frame(&text_delta)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("text delta should have event id");

        let response = forward_with_noop_writers(&bridge, &trusted_resume_headers(&last_event_id))
            .await
            .expect("cached suffix should bypass upstream failure");
        let text = response_text(response).await;

        assert!(!text.contains("\"type\":\"error\""));
        assert!(!text.contains("\"content\":\"cached\""));
        assert!(text.contains("\"type\":\"turn_complete\""));
    }

    #[tokio::test]
    async fn request_send_failure_replays_persisted_suffix_after_cache_miss() {
        use tokio::sync::Mutex;

        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let bridge = HttpChatTurnBridge::new(
            "http://[::1".to_string(),
            Arc::new(Mutex::new(SessionCache::new(1000, 86400.0))),
        )
        .with_persisted_replay_window_store(persisted_replay_window_store.clone());
        let mut sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let turn_complete = render_stream_event_bytes(
            serde_json::json!({"type": "turn_complete"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let mut window = BridgeReplayWindow::default();
        assert!(window.append_frame(&session_info));
        assert!(window.append_frame(&text_delta));
        assert!(window.append_frame(&turn_complete));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
                &window,
            )
            .await
            .expect("persisted replay window should store");
        let last_event_id = parse_sse_json_frame(&text_delta)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("text delta should have event id");

        let response = forward_with_noop_writers(&bridge, &trusted_resume_headers(&last_event_id))
            .await
            .expect("persisted suffix should bypass upstream failure");
        let text = response_text(response).await;

        assert!(!text.contains("\"type\":\"error\""));
        assert!(!text.contains("\"content\":\"cached\""));
        assert!(text.contains("\"type\":\"turn_complete\""));
    }

    #[tokio::test]
    async fn append_bridge_replay_frame_persists_incomplete_scope_window() {
        let cache = Arc::new(tokio::sync::Mutex::new(SessionCache::new(1000, 86400.0)));
        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let mut sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );

        append_bridge_replay_frame(
            cache,
            Some(persisted_replay_window_store.clone()),
            Some("sess-1"),
            Some("run-1"),
            None,
            &session_info,
        )
        .await;

        let window = persisted_replay_window_store
            .load_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
            )
            .await
            .expect("load should succeed")
            .expect("incomplete window should persist");
        assert!(!window.complete);
        assert_eq!(window.frames.len(), 1);
        assert_eq!(window.last_persisted_sequence, Some(1));
    }

    #[tokio::test]
    async fn request_send_failure_replays_persisted_incomplete_suffix() {
        use tokio::sync::Mutex;

        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let bridge = HttpChatTurnBridge::new(
            "http://[::1".to_string(),
            Arc::new(Mutex::new(SessionCache::new(1000, 86400.0))),
        )
        .with_persisted_replay_window_store(persisted_replay_window_store.clone());

        let mut sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached-incomplete"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let mut window = BridgeReplayWindow::default();
        assert!(window.append_frame(&session_info));
        assert!(window.append_frame(&text_delta));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
                &window,
            )
            .await
            .expect("persisted replay window should store");
        let last_event_id = parse_sse_json_frame(&session_info)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("session info should have event id");

        let response = forward_with_noop_writers(&bridge, &trusted_resume_headers(&last_event_id))
            .await
            .expect("persisted incomplete suffix should bypass upstream failure");
        let text = response_text(response).await;

        assert!(!text.contains("\"type\":\"error\""));
        assert!(text.contains("\"content\":\"cached-incomplete\""));
        assert!(!text.contains("\"type\":\"turn_complete\""));
    }

    #[tokio::test]
    async fn retryable_non_sse_response_replays_persisted_incomplete_suffix() {
        use axum::Router;
        use axum::http::header;
        use axum::routing::post;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let app = Router::new().route(
            "/",
            post(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "bridge warming up",
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let bridge = HttpChatTurnBridge::new(
            format!("http://{addr}/"),
            Arc::new(Mutex::new(SessionCache::new(1000, 86400.0))),
        )
        .with_persisted_replay_window_store(persisted_replay_window_store.clone());

        let mut sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached-incomplete"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let mut window = BridgeReplayWindow::default();
        assert!(window.append_frame(&session_info));
        assert!(window.append_frame(&text_delta));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
                &window,
            )
            .await
            .expect("persisted replay window should store");
        let last_event_id = parse_sse_json_frame(&session_info)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("session info should have event id");

        let response = forward_with_noop_writers(&bridge, &trusted_resume_headers(&last_event_id))
            .await
            .expect("retryable non-sse failure should replay durable suffix");
        let text = response_text(response).await;

        assert!(!text.contains("\"type\":\"error\""));
        assert!(text.contains("\"content\":\"cached-incomplete\""));
        assert!(!text.contains("\"type\":\"turn_complete\""));
    }

    #[tokio::test]
    async fn request_timeout_non_sse_replays_persisted_incomplete_suffix() {
        use axum::Router;
        use axum::http::header;
        use axum::routing::post;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let app = Router::new().route(
            "/",
            post(|| async {
                (
                    StatusCode::REQUEST_TIMEOUT,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "upstream timed out",
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let bridge = HttpChatTurnBridge::new(
            format!("http://{addr}/"),
            Arc::new(Mutex::new(SessionCache::new(1000, 86400.0))),
        )
        .with_persisted_replay_window_store(persisted_replay_window_store.clone());

        let mut sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached-incomplete"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let mut window = BridgeReplayWindow::default();
        assert!(window.append_frame(&session_info));
        assert!(window.append_frame(&text_delta));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
                &window,
            )
            .await
            .expect("persisted replay window should store");
        let last_event_id = parse_sse_json_frame(&session_info)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("session info should have event id");

        let response = forward_with_noop_writers(&bridge, &trusted_resume_headers(&last_event_id))
            .await
            .expect("request-timeout failure should replay durable suffix");
        let text = response_text(response).await;

        assert!(!text.contains("\"type\":\"error\""));
        assert!(text.contains("\"content\":\"cached-incomplete\""));
        assert!(!text.contains("\"type\":\"turn_complete\""));
    }

    #[tokio::test]
    async fn non_sse_client_error_keeps_error_sse_even_with_persisted_incomplete_suffix() {
        use axum::Router;
        use axum::http::header;
        use axum::routing::post;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let app = Router::new().route(
            "/",
            post(|| async {
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "bad input",
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let bridge = HttpChatTurnBridge::new(
            format!("http://{addr}/"),
            Arc::new(Mutex::new(SessionCache::new(1000, 86400.0))),
        )
        .with_persisted_replay_window_store(persisted_replay_window_store.clone());

        let mut sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached-incomplete"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let mut window = BridgeReplayWindow::default();
        assert!(window.append_frame(&session_info));
        assert!(window.append_frame(&text_delta));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
                &window,
            )
            .await
            .expect("persisted replay window should store");
        let last_event_id = parse_sse_json_frame(&session_info)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("session info should have event id");

        let response = forward_with_noop_writers(&bridge, &trusted_resume_headers(&last_event_id))
            .await
            .expect("client error should still produce bridge error event");
        let text = response_text(response).await;

        assert!(text.contains("\"type\":\"error\""), "{text}");
        assert!(text.contains("VALIDATION_ERROR"), "{text}");
        assert!(
            !text.contains("\"content\":\"cached-incomplete\""),
            "{text}"
        );
    }

    #[tokio::test]
    async fn non_sse_success_replays_persisted_incomplete_suffix() {
        use axum::Router;
        use axum::http::header;
        use axum::routing::post;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let app = Router::new().route(
            "/",
            post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "not an sse stream",
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let bridge = HttpChatTurnBridge::new(
            format!("http://{addr}/"),
            Arc::new(Mutex::new(SessionCache::new(1000, 86400.0))),
        )
        .with_persisted_replay_window_store(persisted_replay_window_store.clone());

        let mut sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached-incomplete"}),
            &mut sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let mut window = BridgeReplayWindow::default();
        assert!(window.append_frame(&session_info));
        assert!(window.append_frame(&text_delta));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
                &window,
            )
            .await
            .expect("persisted replay window should store");
        let last_event_id = parse_sse_json_frame(&session_info)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("session info should have event id");

        let response = forward_with_noop_writers(&bridge, &trusted_resume_headers(&last_event_id))
            .await
            .expect("non-sse success should replay durable suffix");
        let text = response_text(response).await;

        assert!(!text.contains("\"type\":\"error\""));
        assert!(text.contains("\"content\":\"cached-incomplete\""));
        assert!(!text.contains("\"type\":\"turn_complete\""));
    }

    #[tokio::test]
    async fn successful_resume_splices_persisted_incomplete_suffix_before_live_stream() {
        use axum::Router;
        use axum::http::header;
        use axum::routing::post;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let seen_last_event_id = Arc::new(Mutex::new(None::<String>));
        let mut upstream_sequence = 0u64;
        let upstream_cached = String::from_utf8(
            render_stream_event_bytes(
                serde_json::json!({"type": "text_delta", "content": "cached-incomplete"}),
                &mut upstream_sequence,
                Some("sess-1"),
                Some("run-1"),
                None,
            )
            .to_vec(),
        )
        .expect("cached frame should be utf8");
        let upstream_live = String::from_utf8(
            render_stream_event_bytes(
                serde_json::json!({"type": "text_delta", "content": "live"}),
                &mut upstream_sequence,
                Some("sess-1"),
                Some("run-1"),
                None,
            )
            .to_vec(),
        )
        .expect("live frame should be utf8");
        let upstream_turn_complete = String::from_utf8(
            render_stream_event_bytes(
                serde_json::json!({"type": "turn_complete"}),
                &mut upstream_sequence,
                Some("sess-1"),
                Some("run-1"),
                None,
            )
            .to_vec(),
        )
        .expect("turn_complete frame should be utf8");
        let upstream_body = format!("{upstream_cached}{upstream_live}{upstream_turn_complete}");

        let app = Router::new().route(
            "/",
            post({
                let upstream_body = upstream_body.clone();
                let seen_last_event_id = seen_last_event_id.clone();
                move |headers: HeaderMap| {
                    let upstream_body = upstream_body.clone();
                    let seen_last_event_id = seen_last_event_id.clone();
                    async move {
                        *seen_last_event_id.lock().await = headers
                            .get("last-event-id")
                            .and_then(|value| value.to_str().ok())
                            .map(ToString::to_string);
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            upstream_body,
                        )
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let bridge = HttpChatTurnBridge::new(
            format!("http://{addr}/"),
            Arc::new(Mutex::new(SessionCache::new(1000, 86400.0))),
        )
        .with_persisted_replay_window_store(persisted_replay_window_store.clone());

        let mut persisted_sequence = 0u64;
        let session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut persisted_sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let cached = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached-incomplete"}),
            &mut persisted_sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let mut window = BridgeReplayWindow::default();
        assert!(window.append_frame(&session_info));
        assert!(window.append_frame(&cached));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
                &window,
            )
            .await
            .expect("persisted replay window should store");

        let last_event_id = parse_sse_json_frame(&session_info)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("session info should have event id");
        let expected_upstream_last_event_id = parse_sse_json_frame(&cached)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("cached frame should have event id");

        let response = forward_with_noop_writers(&bridge, &trusted_resume_headers(&last_event_id))
            .await
            .expect("successful resume should stream");
        let text = response_text(response).await;

        assert_eq!(
            text.matches("\"content\":\"cached-incomplete\"").count(),
            1,
            "{text}"
        );
        assert_eq!(text.matches("\"content\":\"live\"").count(), 1, "{text}");
        assert_eq!(
            text.matches("\"type\":\"turn_complete\"").count(),
            1,
            "{text}"
        );
        assert_eq!(
            text.matches("\"type\":\"session_info\"").count(),
            0,
            "{text}"
        );
        let cached_index = text
            .find("\"content\":\"cached-incomplete\"")
            .expect("cached splice should appear");
        let live_index = text
            .find("\"content\":\"live\"")
            .expect("live frame should appear");
        let turn_complete_index = text
            .find("\"type\":\"turn_complete\"")
            .expect("turn complete should appear");
        assert!(cached_index < live_index);
        assert!(live_index < turn_complete_index);
        assert_eq!(
            seen_last_event_id.lock().await.as_deref(),
            Some(expected_upstream_last_event_id.as_str())
        );
    }

    #[tokio::test]
    async fn request_send_failure_replays_scope_specific_persisted_suffix_after_newer_scope_persisted()
     {
        use tokio::sync::Mutex;

        let persisted_replay_window_store = Arc::new(InMemoryBridgeReplayWindowStore::default());
        let bridge = HttpChatTurnBridge::new(
            "http://[::1".to_string(),
            Arc::new(Mutex::new(SessionCache::new(1000, 86400.0))),
        )
        .with_persisted_replay_window_store(persisted_replay_window_store.clone());

        let mut run_1_sequence = 0u64;
        let run_1_session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-1"}),
            &mut run_1_sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let run_1_text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached-run-1"}),
            &mut run_1_sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let run_1_turn_complete = render_stream_event_bytes(
            serde_json::json!({"type": "turn_complete"}),
            &mut run_1_sequence,
            Some("sess-1"),
            Some("run-1"),
            None,
        );
        let mut run_1_window = BridgeReplayWindow::default();
        assert!(run_1_window.append_frame(&run_1_session_info));
        assert!(run_1_window.append_frame(&run_1_text_delta));
        assert!(run_1_window.append_frame(&run_1_turn_complete));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-1"), None),
                &run_1_window,
            )
            .await
            .expect("run-1 persisted replay window should store");
        let run_1_last_event_id = parse_sse_json_frame(&run_1_text_delta)
            .and_then(|event| {
                event
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .expect("run-1 text delta should have event id");

        let mut run_2_sequence = 0u64;
        let run_2_session_info = render_stream_event_bytes(
            serde_json::json!({"type": "session_info", "session_id": "sess-1", "run_id": "run-2"}),
            &mut run_2_sequence,
            Some("sess-1"),
            Some("run-2"),
            None,
        );
        let run_2_text_delta = render_stream_event_bytes(
            serde_json::json!({"type": "text_delta", "content": "cached-run-2"}),
            &mut run_2_sequence,
            Some("sess-1"),
            Some("run-2"),
            None,
        );
        let run_2_turn_complete = render_stream_event_bytes(
            serde_json::json!({"type": "turn_complete"}),
            &mut run_2_sequence,
            Some("sess-1"),
            Some("run-2"),
            None,
        );
        let mut run_2_window = BridgeReplayWindow::default();
        assert!(run_2_window.append_frame(&run_2_session_info));
        assert!(run_2_window.append_frame(&run_2_text_delta));
        assert!(run_2_window.append_frame(&run_2_turn_complete));
        persisted_replay_window_store
            .persist_latest_window(
                "sess-1",
                &bridge_replay_window_key("sess-1", Some("run-2"), None),
                &run_2_window,
            )
            .await
            .expect("run-2 persisted replay window should store");

        let response =
            forward_with_noop_writers(&bridge, &trusted_resume_headers(&run_1_last_event_id))
                .await
                .expect("scope-specific persisted suffix should bypass upstream failure");
        let text = response_text(response).await;

        assert!(!text.contains("\"type\":\"error\""));
        assert!(!text.contains("\"content\":\"cached-run-1\""));
        assert!(!text.contains("\"content\":\"cached-run-2\""));
        assert!(text.contains("\"type\":\"turn_complete\""));
    }

    #[tokio::test]
    async fn unavailable_bridge_preserves_trusted_session_info() {
        let response =
            forward_with_noop_writers(&UnavailableChatTurnBridge, &trusted_identity_headers())
                .await
                .expect("disabled bridge should normalize to SSE");
        let text = response_text(response).await;

        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("chat turn bridge disabled"));
    }

    #[tokio::test]
    async fn non_sse_error_response_preserves_trusted_session_info() {
        use axum::Router;
        use axum::http::header;
        use axum::routing::post;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let app = Router::new().route(
            "/",
            post(|| async {
                (
                    StatusCode::BAD_GATEWAY,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "upstream exploded",
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let cache = Arc::new(Mutex::new(SessionCache::default()));
        let bridge = HttpChatTurnBridge::new(format!("http://{addr}/"), cache);

        let response = forward_with_noop_writers(&bridge, &trusted_identity_headers())
            .await
            .expect("non-sse error should normalize to SSE");
        let text = response_text(response).await;

        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
    }

    #[tokio::test]
    async fn request_timeout_error_sse_is_retryable_upstream_error() {
        let response = bridge_error_sse_response(
            StatusCode::REQUEST_TIMEOUT,
            bridge_error_sse_message(StatusCode::REQUEST_TIMEOUT, "upstream timed out"),
            Some("sess-1"),
            Some("run-1"),
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
        assert!(text.contains("\"retryable\":true"));
        assert!(text.contains("upstream timed out"));
    }

    #[tokio::test]
    async fn non_sse_error_body_excerpt_is_capped() {
        use futures_util::stream;

        let chunk = "x".repeat((MAX_BRIDGE_ERROR_BODY_BYTES / 2) + 10);
        let text = read_bridge_error_body_excerpt(
            stream::iter(vec![
                Ok::<Bytes, std::io::Error>(Bytes::from(chunk.clone())),
                Ok::<Bytes, std::io::Error>(Bytes::from(chunk)),
            ]),
            MAX_BRIDGE_ERROR_BODY_BYTES,
        )
        .await;

        assert!(text.len() <= MAX_BRIDGE_ERROR_BODY_BYTES + "\n… [truncated]".len());
        assert!(text.ends_with("\n… [truncated]"));
    }

    #[tokio::test]
    async fn bridge_error_sse_response_wraps_plaintext_body() {
        let response = bridge_error_sse_response(
            StatusCode::BAD_GATEWAY,
            bridge_error_sse_message(StatusCode::BAD_GATEWAY, "upstream exploded"),
            None,
            None,
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
        assert!(text.contains("upstream exploded"));
    }

    #[tokio::test]
    async fn bridge_response_stream_cancels_token_when_body_drops() {
        let token = Arc::new(CancellationToken::new());
        let body = Body::from_stream(BridgeResponseStream::new(
            futures_util::stream::pending::<Result<Bytes, std::io::Error>>(),
            Some(token.clone()),
        ));

        assert!(!token.is_cancelled());
        drop(body);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn bridge_error_sse_response_prepends_trusted_session_info() {
        let response = bridge_error_sse_response(
            StatusCode::BAD_REQUEST,
            "bad request",
            Some("sess-1"),
            Some("run-1"),
        );
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let session_info_pos = text
            .find("\"type\":\"session_info\"")
            .expect("session_info event");
        let error_pos = text.find("\"type\":\"error\"").expect("error event");
        assert!(session_info_pos < error_pos);
        assert!(text.contains("\"session_id\":\"sess-1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
    }

    #[tokio::test]
    async fn passthrough_bridge_keeps_upstream_session_info_when_untrusted() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"session_info\",\"session_id\":\"upstream-s1\"}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"upstream-s1\""));
    }

    #[tokio::test]
    async fn passthrough_bridge_keeps_upstream_bridge_state_when_untrusted() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello\"}}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"bridge_state\""));
        assert!(text.contains("\"full_text\":\"hello\""));
    }

    #[tokio::test]
    async fn passthrough_bridge_keeps_buffered_bridge_state_when_untrusted() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello\"}}",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"bridge_state\""));
        assert!(text.contains("\"full_text\":\"hello\""));
    }

    #[tokio::test]
    async fn bridge_stream_read_error_is_normalized_to_sse_error() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let reqwest_error = reqwest::Client::new()
            .get("http://[::1")
            .build()
            .expect_err("invalid url should build reqwest error");

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Err::<Bytes, reqwest::Error>(reqwest_error)]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should stay readable");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
        assert!(text.contains("Failed to read bridge response"));
    }

    #[tokio::test]
    async fn bridge_stream_read_error_flushes_pending_warning_before_error() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let reqwest_error = reqwest::Client::new()
            .get("http://[::1")
            .build()
            .expect_err("invalid url should build reqwest error");

        let filtered = filter_bridge_state_events(
            stream::iter(vec![
                Ok::<Bytes, reqwest::Error>(Bytes::from(
                    "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"firewall_warning_claims_failed\":2}\n\n",
                )),
                Err::<Bytes, reqwest::Error>(reqwest_error),
            ]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should stay readable");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let warning_pos = text.find("\"type\":\"warning\"").expect("warning event");
        let error_pos = text.find("\"type\":\"error\"").expect("error event");
        let turn_complete_pos = text
            .find("\"type\":\"turn_complete\"")
            .expect("turn_complete event");
        assert!(warning_pos < error_pos);
        assert!(error_pos < turn_complete_pos);
        assert!(text.contains("\"code\":\"UPSTREAM_ERROR\""));
    }

    #[tokio::test]
    async fn buffered_final_session_info_is_suppressed_when_trusted() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"session_info\",\"session_id\":\"upstream-s1\"}",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("trusted-s1".to_string()),
            None,
            None,
            Some("run-1".to_string()),
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert_eq!(text.matches("\"type\":\"session_info\"").count(), 1);
        assert!(text.contains("\"session_id\":\"trusted-s1\""));
        assert!(!text.contains("\"session_id\":\"upstream-s1\""));
        assert!(text.contains("\"run_id\":\"run-1\""));
    }

    #[tokio::test]
    async fn guard_error_emits_local_turn_complete_without_upstream_terminal() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello hello hello hello hello hello hello hello\"}}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"MODEL_DEGRADED\""));
        assert_eq!(text.matches("\"type\":\"turn_complete\"").count(), 1);
    }

    #[tokio::test]
    async fn buffered_guard_error_emits_local_turn_complete_without_upstream_terminal() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello hello hello hello hello hello hello hello\"}}",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"MODEL_DEGRADED\""));
        assert_eq!(text.matches("\"type\":\"turn_complete\"").count(), 1);
    }

    #[tokio::test]
    async fn buffered_guard_error_suppresses_unterminated_upstream_turn_complete() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![
                Ok::<Bytes, reqwest::Error>(Bytes::from(
                    "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"tail_update_args\":{\"full_text\":\"hello hello hello hello hello hello hello hello\"}}\n\n",
                )),
                Ok::<Bytes, reqwest::Error>(Bytes::from(
                    "data: {\"type\":\"turn_complete\",\"message\":\"upstream\"}",
                )),
            ]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert_eq!(text.matches("\"type\":\"turn_complete\"").count(), 1);
        assert!(!text.contains("\"message\":\"upstream\""));
    }

    #[tokio::test]
    async fn guard_error_flushes_pending_warning_before_terminal_events() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[],\"firewall_warning_claims_failed\":2,\"tail_update_args\":{\"full_text\":\"hello hello hello hello hello hello hello hello\"}}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let warning_pos = text.find("\"type\":\"warning\"").expect("warning event");
        let error_pos = text.find("\"type\":\"error\"").expect("error event");
        let turn_complete_pos = text
            .find("\"type\":\"turn_complete\"")
            .expect("turn_complete event");
        assert!(warning_pos < error_pos);
        assert!(error_pos < turn_complete_pos);
    }

    #[tokio::test]
    async fn max_tool_rounds_flushes_pending_warning_before_forced_completion() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let mut frames = Vec::new();
        for idx in 0..=max_tool_rounds() {
            let warning = if idx == max_tool_rounds() {
                ",\"firewall_warning_claims_failed\":2"
            } else {
                ""
            };
            frames.push(Ok::<Bytes, reqwest::Error>(Bytes::from(format!(
                "data: {{\"type\":\"bridge_state\",\"tool_sigs\":[[\"run_build_test:{{}}\"]]{warning}}}\n\n"
            ))));
        }

        let filtered = filter_bridge_state_events(
            stream::iter(frames),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let warning_pos = text.find("\"type\":\"warning\"").expect("warning event");
        let turn_complete_pos = text
            .find("\"type\":\"turn_complete\"")
            .expect("turn_complete event");
        assert!(warning_pos < turn_complete_pos);
        assert!(text.contains("\"max_rounds_exceeded\":true"));
    }

    #[tokio::test]
    async fn raw_explain_frame_is_preserved_without_bridge_state() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"explain\",\"total_ms\":12,\"tools_selected\":1,\"tools_available\":2}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"explain\""));
        assert!(text.contains("\"total_ms\":12"));
    }

    #[tokio::test]
    async fn raw_warning_frame_is_preserved_without_bridge_state() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let filtered = filter_bridge_state_events(
            stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"warning\",\"message\":\"approaching limit\"}\n\n",
            ))]),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"warning\""));
        assert!(text.contains("approaching limit"));
    }

    #[tokio::test]
    async fn buffered_final_bridge_state_still_enforces_max_tool_rounds() {
        use futures_util::stream;
        use tokio::sync::Mutex;

        let mut frames = Vec::new();
        for _ in 0..max_tool_rounds() {
            frames.push(Ok::<Bytes, reqwest::Error>(Bytes::from(
                "data: {\"type\":\"bridge_state\",\"tool_sigs\":[[\"run_build_test:{}\"]]}\n\n",
            )));
        }
        frames.push(Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"type\":\"bridge_state\",\"tool_sigs\":[[\"run_build_test:{}\"]]}",
        )));

        let filtered = filter_bridge_state_events(
            stream::iter(frames),
            Arc::new(Mutex::new(SessionCache::default())),
            Some("sess-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
            Arc::new(crate::turn::services::NoopTurnToolEventWriter),
            Arc::new(crate::turn::services::NoopTurnHookDbWriter),
            Arc::new(InMemoryTurnReflectionStateStore::default()),
            Arc::new(NoopTurnReflectionLessonWriter),
            Arc::new(NoopTurnObserverWorker),
            Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
            Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
            None,
        );

        let body = axum::body::to_bytes(Body::from_stream(filtered), 1024 * 1024)
            .await
            .expect("body should read");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("\"type\":\"turn_complete\""));
        assert!(text.contains("\"max_rounds_exceeded\":true"));
    }
}
