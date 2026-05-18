use super::*;
use astra_core::{STATUS_CANCELLED, is_duplicate_key_error};
use astra_services::{
    DatabaseSessionArtifactStore, DatabaseStateProjectionStore, PresignedArtifactDownload,
    SessionArtifactJsonStore, UserAnchorMemoryItem, build_presigned_artifact_download,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

const DEFAULT_TRANSCRIPT_LIMIT: u32 = 50;
const MAX_TRANSCRIPT_LIMIT: u32 = 200;
const DEVICE_LEASE_TTL_HOURS: i64 = 2;

#[derive(Deserialize, Default)]
pub(super) struct SessionStateQuery {
    #[serde(default)]
    pub known_state_revision: u64,
    #[serde(default)]
    pub known_revision_hash: Option<String>,
    #[serde(default)]
    pub client_cache_empty: bool,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub device_fingerprint: Option<String>,
}

#[derive(Serialize)]
pub(super) struct SessionStateResponse {
    pub session_id: String,
    pub state_revision: StateRevisionResponse,
    pub transcript_high_watermark: i64,
    pub active_run: Option<ActiveRunProjection>,
    pub anchor_memory: Vec<UserAnchorMemoryResponse>,
    pub replay_required: bool,
    pub transcript_replay_required: bool,
    pub run_event_replay_required: bool,
}

#[derive(Serialize)]
pub(super) struct StateRevisionResponse {
    pub monotonic_id: u64,
    pub revision_hash: String,
}

#[derive(Serialize)]
pub(super) struct ActiveRunProjection {
    pub run_id: String,
    pub run_event_high_watermark: i64,
    pub replay_required: bool,
    pub replay_start_event_idx: i64,
}

#[derive(Serialize)]
pub(super) struct UserAnchorMemoryResponse {
    pub item_id: String,
    pub category: String,
    pub item_key: String,
    pub summary_text: Option<String>,
    pub token_estimate: u32,
}

fn user_anchor_memory_response(item: UserAnchorMemoryItem) -> UserAnchorMemoryResponse {
    UserAnchorMemoryResponse {
        item_id: item.item_id,
        category: item.category,
        item_key: item.item_key,
        summary_text: item.summary_text,
        token_estimate: item.token_estimate,
    }
}

#[derive(Deserialize, Default)]
pub(super) struct TranscriptQuery {
    pub before_seq: Option<i64>,
    #[serde(default = "default_transcript_limit")]
    pub limit: u32,
}

#[derive(Serialize)]
pub(super) struct TranscriptResponse {
    pub session_id: String,
    pub items: Vec<TranscriptItemResponse>,
    pub next_before_seq: Option<i64>,
    pub has_more: bool,
}

#[derive(Serialize)]
pub(super) struct TranscriptItemResponse {
    pub session_id: String,
    pub item_seq: i64,
    pub run_id: Option<String>,
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_status: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Default)]
struct TranscriptReasoningProjection {
    text: String,
    done: bool,
}

impl TranscriptReasoningProjection {
    fn append_delta(&mut self, delta: &str) {
        if delta.is_empty() || self.text.ends_with(delta) {
            return;
        }
        if !self.text.is_empty() && delta.starts_with(&self.text) {
            self.text.clear();
        }
        self.text.push_str(delta);
    }

    fn reasoning(&self) -> Option<String> {
        let trimmed = self.text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn status(&self) -> Option<String> {
        self.reasoning()
            .map(|_| if self.done { "complete" } else { "streaming" }.to_string())
    }
}

#[derive(Serialize)]
pub(super) struct DeviceLeaseResponse {
    pub lease_id: String,
    pub session_id: String,
    pub device_id: String,
    pub device_fingerprint: String,
    pub trust_level: String,
    pub status: String,
    pub last_monotonic_id: i64,
    pub expires_at: String,
}

#[derive(Serialize)]
pub(super) struct DeviceListResponse {
    pub session_id: String,
    pub devices: Vec<DeviceLeaseResponse>,
}

#[derive(Deserialize, Default)]
pub(super) struct DeviceRevokeRequest {
    pub lease_id: Option<String>,
    pub device_id: Option<String>,
    #[serde(default)]
    pub expected_last_monotonic_id: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, Default)]
pub(super) struct DeviceTrustRequest {
    pub device_id: String,
    #[serde(default)]
    pub step_up_confirmation: bool,
    #[serde(default)]
    pub expected_last_monotonic_id: Option<i64>,
}

#[derive(Serialize)]
pub(super) struct DeviceRevokeResponse {
    pub event: DeviceLeaseEndedPayload,
    pub idempotent: bool,
}

#[derive(Serialize)]
pub(super) struct DeviceTrustResponse {
    pub lease: DeviceLeaseResponse,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct DeviceLeaseEndedPayload {
    pub r#type: String,
    pub lease_id: String,
    pub session_id: String,
    pub device_id: String,
    pub device_fingerprint: String,
    pub reason: String,
    pub ended_at_server: String,
}

fn default_transcript_limit() -> u32 {
    DEFAULT_TRANSCRIPT_LIMIT
}

pub(super) async fn create_session_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SessionCreateRequest>,
) -> Result<(StatusCode, Json<SessionResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = super::session_quota::create_session_with_resource_quota(
        &state,
        user.user_id,
        SessionCreateRequestData {
            agent_id: request.agent_id,
            title: request.title,
            metadata: request.metadata,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(SessionResponse::from(session))))
}

pub(super) async fn list_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<SessionListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let sessions = state
        .session_service
        .list_sessions(SessionListFilter {
            user_id: user.user_id,
            agent_id: query.agent_id,
            status: query.session_status,
            limit: query.limit,
            offset: query.offset,
        })
        .await?;
    Ok(Json(SessionListResponse::from(sessions)))
}

pub(super) async fn get_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id)
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn get_session_state_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionStateQuery>,
) -> Result<Json<SessionStateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
    let device_fingerprint = query
        .device_fingerprint
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown-device")
        .to_string();
    if let Some(device_id) = query.device_id.as_deref() {
        ensure_device_lease(
            pool,
            &session.user_id,
            &session.session_id,
            device_id,
            &device_fingerprint,
        )
        .await?;
    }

    let transcript_high_watermark = transcript_high_watermark(pool, &session.session_id).await?;
    let active_run = active_run_projection(pool, &session.session_id).await?;
    let run_event_high_watermark = active_run
        .as_ref()
        .map(|run| run.run_event_high_watermark)
        .unwrap_or(0);
    let monotonic_id = state_monotonic_id(
        query.known_state_revision,
        transcript_high_watermark,
        run_event_high_watermark,
    );
    let state_projection_hash = sha256_hex(
        format!(
            "{}|{}|{}|{}",
            session.session_id, session.status, transcript_high_watermark, run_event_high_watermark
        )
        .as_bytes(),
    );
    let revision_hash = revision_hash(
        &session.session_id,
        monotonic_id,
        &device_fingerprint,
        transcript_high_watermark,
        run_event_high_watermark,
        &state_projection_hash,
    );

    if query.known_state_revision == monotonic_id
        && let Some(known_hash) = query.known_revision_hash.as_deref()
        && known_hash != revision_hash
    {
        return Err(error_response(
            StatusCode::CONFLICT,
            "state revision hash mismatch; local cache rollback detected",
        ));
    }

    persist_session_state_revision(
        pool,
        SessionStateRevisionWrite {
            session_id: &session.session_id,
            user_id: &session.user_id,
            monotonic_id: monotonic_id as i64,
            revision_hash: &revision_hash,
            device_fingerprint: &device_fingerprint,
            transcript_high_watermark,
            run_event_high_watermark,
            state_projection_hash: &state_projection_hash,
        },
    )
    .await
    .map_err(|error| {
        internal_error(format!(
            "persist session state revision failed for session {}: {error}",
            session.session_id
        ))
    })?;

    let cold_start = query.known_state_revision == 0 || query.client_cache_empty;
    let transcript_replay_required = cold_start && transcript_high_watermark > 0;
    let run_event_replay_required = cold_start && run_event_high_watermark > 0;
    let replay_required = transcript_replay_required || run_event_replay_required;
    let active_run = active_run.map(|mut run| {
        run.replay_required = run_event_replay_required;
        run.replay_start_event_idx = if run_event_replay_required {
            0
        } else {
            run.replay_start_event_idx
        };
        run
    });
    let anchor_memory = DatabaseStateProjectionStore::new(pool.clone())
        .load_user_anchor_memory(&session.user_id, 400)
        .await
        .map_err(|error| {
            internal_error(format!(
                "load user anchor memory for session {} failed: {error}",
                session.session_id
            ))
        })?
        .into_iter()
        .map(user_anchor_memory_response)
        .collect();

    Ok(Json(SessionStateResponse {
        session_id: session.session_id,
        state_revision: StateRevisionResponse {
            monotonic_id,
            revision_hash,
        },
        transcript_high_watermark,
        active_run,
        anchor_memory,
        replay_required,
        transcript_replay_required,
        run_event_replay_required,
    }))
}

pub(super) async fn get_session_transcript_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<TranscriptQuery>,
) -> Result<Json<TranscriptResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
    let limit = query.limit.clamp(1, MAX_TRANSCRIPT_LIMIT);
    let before_seq = query.before_seq.unwrap_or(i64::MAX);
    let rows = sqlx::query(
        "SELECT session_id, item_seq, run_id, role, content,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at
         FROM session_transcript_items
         WHERE session_id = ? AND item_seq < ?
         ORDER BY item_seq DESC
         LIMIT ?",
    )
    .bind(&session_id)
    .bind(before_seq)
    .bind(i64::from(limit))
    .fetch_all(pool.get())
    .await
    .map_err(internal_error)?;

    let run_ids = transcript_assistant_run_ids(&rows);
    let reasoning_by_run = load_transcript_reasoning_by_run(pool, &session_id, &run_ids)
        .await
        .map_err(|error| {
            internal_error(format!(
                "load transcript reasoning failed for session {session_id}: {error}"
            ))
        })?;

    let mut items = rows
        .into_iter()
        .map(|row| {
            let run_id = row.try_get::<Option<String>, _>("run_id").ok().flatten();
            let role = row.try_get::<String, _>("role").unwrap_or_default();
            let reasoning = if role == "assistant" {
                run_id
                    .as_deref()
                    .and_then(|id| reasoning_by_run.get(id))
                    .and_then(TranscriptReasoningProjection::reasoning)
            } else {
                None
            };
            let reasoning_status = if role == "assistant" {
                run_id
                    .as_deref()
                    .and_then(|id| reasoning_by_run.get(id))
                    .and_then(TranscriptReasoningProjection::status)
            } else {
                None
            };
            TranscriptItemResponse {
                session_id: row.try_get("session_id").unwrap_or_default(),
                item_seq: row.try_get("item_seq").unwrap_or_default(),
                run_id,
                role,
                content: row.try_get("content").unwrap_or_default(),
                reasoning,
                reasoning_status,
                created_at: row.try_get("created_at").unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    items.reverse();
    let next_before_seq = items.first().map(|item| item.item_seq);
    let has_more = items.len() == limit as usize && next_before_seq.unwrap_or(0) > 1;
    Ok(Json(TranscriptResponse {
        session_id,
        items,
        next_before_seq,
        has_more,
    }))
}

fn transcript_assistant_run_ids(rows: &[sqlx::mysql::MySqlRow]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut run_ids = Vec::new();
    for row in rows {
        let role = row.try_get::<String, _>("role").unwrap_or_default();
        if role != "assistant" {
            continue;
        }
        let Some(run_id) = row.try_get::<Option<String>, _>("run_id").ok().flatten() else {
            continue;
        };
        if seen.insert(run_id.clone()) {
            run_ids.push(run_id);
        }
    }
    run_ids
}

async fn load_transcript_reasoning_by_run(
    pool: &SharedPool,
    session_id: &str,
    run_ids: &[String],
) -> Result<HashMap<String, TranscriptReasoningProjection>, sqlx::Error> {
    if run_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut query = QueryBuilder::<sqlx::MySql>::new(
        "SELECT run_id, payload_json
         FROM agent_run_events
         WHERE session_id = ",
    );
    query.push_bind(session_id);
    query.push(" AND run_id IN (");
    {
        let mut separated = query.separated(", ");
        for run_id in run_ids {
            separated.push_bind(run_id);
        }
    }
    query.push(
        ") AND event_type IN (
             'reasoning_delta',
             'reasoning_message_content',
             'reasoning_done',
             'thinking_delta',
             'thinking_done'
         )
         ORDER BY run_id ASC, event_idx ASC",
    );

    let rows = query.build().fetch_all(pool.get()).await?;
    let mut by_run: HashMap<String, TranscriptReasoningProjection> = HashMap::new();
    for row in rows {
        let run_id = row.try_get::<String, _>("run_id")?;
        let payload_json = row.try_get::<String, _>("payload_json")?;
        let payload = serde_json::from_str::<Value>(&payload_json).map_err(|error| {
            sqlx::Error::Protocol(format!(
                "invalid reasoning event payload for run {run_id}: {error}"
            ))
        })?;
        let projection = by_run.entry(run_id).or_default();
        apply_reasoning_event_payload(projection, &payload);
    }
    Ok(by_run)
}

fn apply_reasoning_event_payload(projection: &mut TranscriptReasoningProjection, payload: &Value) {
    match reasoning_event_type(payload) {
        Some("reasoning_delta" | "thinking_delta" | "reasoning_message_content") => {
            if let Some(content) = reasoning_event_content(payload) {
                projection.append_delta(content);
            }
        }
        Some("reasoning_done" | "thinking_done") => {
            projection.done = true;
        }
        _ => {}
    }
}

fn reasoning_event_type(payload: &Value) -> Option<&str> {
    payload
        .get("event_type")
        .or_else(|| payload.get("type"))
        .and_then(Value::as_str)
}

fn reasoning_event_content(payload: &Value) -> Option<&str> {
    payload
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| payload.pointer("/data/content").and_then(Value::as_str))
        .or_else(|| payload.pointer("/data/chunk").and_then(Value::as_str))
        .or_else(|| payload.pointer("/data/reasoning").and_then(Value::as_str))
        .filter(|content| !content.trim().is_empty())
}

pub(super) async fn update_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SessionUpdateRequest>,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: request.title,
                metadata: request.metadata,
                status: request.status,
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn list_session_devices_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DeviceListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
    let rows = sqlx::query(
        "SELECT lease_id, session_id, device_id, device_fingerprint, trust_level,
                status, last_monotonic_id, DATE_FORMAT(expires_at, '%Y-%m-%dT%H:%i:%s') AS expires_at
         FROM session_device_leases
         WHERE session_id = ? AND user_id = ?
         ORDER BY updated_at DESC",
    )
    .bind(&session_id)
    .bind(&user.user_id)
    .fetch_all(pool.get())
    .await
    .map_err(internal_error)?;

    Ok(Json(DeviceListResponse {
        session_id,
        devices: rows.into_iter().map(device_response_from_row).collect(),
    }))
}

pub(super) async fn revoke_session_device_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<DeviceRevokeRequest>,
) -> Result<Json<DeviceRevokeResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;

    let lease = load_device_lease_for_revoke(
        pool,
        &user.user_id,
        &session_id,
        request.lease_id.as_deref(),
        request.device_id.as_deref(),
    )
    .await?;
    if lease.status != "active" {
        let payload = DeviceLeaseEndedPayload {
            r#type: "device_revoked".to_string(),
            lease_id: lease.lease_id,
            session_id: lease.session_id,
            device_id: lease.device_id,
            device_fingerprint: lease.device_fingerprint,
            reason: request
                .reason
                .unwrap_or_else(|| "already_ended".to_string()),
            ended_at_server: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        };
        return Ok(Json(DeviceRevokeResponse {
            event: payload,
            idempotent: true,
        }));
    }

    let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "UPDATE session_device_leases
         SET status = 'revoked', revoked_at = NOW(6), updated_at = NOW(6)
         WHERE lease_id = ",
    );
    update.push_bind(&lease.lease_id);
    update.push(" AND status = 'active'");
    if let Some(expected) = request.expected_last_monotonic_id {
        update.push(" AND last_monotonic_id = ");
        update.push_bind(expected);
    }
    let result = update
        .build()
        .execute(pool.get())
        .await
        .map_err(internal_error)?;
    if result.rows_affected() == 0 {
        return Err(error_response(
            StatusCode::CONFLICT,
            "device lease revoke CAS conflict",
        ));
    }

    let payload = insert_device_lease_event(
        pool,
        &lease,
        "device_revoked",
        request.reason.as_deref().unwrap_or("explicit_revoke"),
    )
    .await?;
    if let Ok(value) = serde_json::to_value(&payload) {
        super::device_lease_sweeper::publish_device_lease_event(value);
    }
    Ok(Json(DeviceRevokeResponse {
        event: payload,
        idempotent: false,
    }))
}

pub(super) async fn trust_session_device_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<DeviceTrustRequest>,
) -> Result<Json<DeviceTrustResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;
    if !request.step_up_confirmation {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "step-up confirmation is required to trust a new device",
        ));
    }
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;

    let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "UPDATE session_device_leases
         SET trust_level = 'trusted', updated_at = NOW(6)
         WHERE session_id = ",
    );
    update.push_bind(&session_id);
    update.push(" AND user_id = ");
    update.push_bind(&user.user_id);
    update.push(" AND device_id = ");
    update.push_bind(&request.device_id);
    update.push(" AND status = 'active' AND trust_level = 'new_device'");
    if let Some(expected) = request.expected_last_monotonic_id {
        update.push(" AND last_monotonic_id = ");
        update.push_bind(expected);
    }
    let result = update
        .build()
        .execute(pool.get())
        .await
        .map_err(internal_error)?;
    if result.rows_affected() == 0 {
        return Err(error_response(
            StatusCode::CONFLICT,
            "device trust CAS conflict or device is already trusted",
        ));
    }

    let row = sqlx::query(
        "SELECT lease_id, session_id, device_id, device_fingerprint, trust_level,
                status, last_monotonic_id, DATE_FORMAT(expires_at, '%Y-%m-%dT%H:%i:%s') AS expires_at
         FROM session_device_leases
         WHERE session_id = ? AND user_id = ? AND device_id = ?
         LIMIT 1",
    )
    .bind(&session_id)
    .bind(&user.user_id)
    .bind(&request.device_id)
    .fetch_one(pool.get())
    .await
    .map_err(internal_error)?;
    Ok(Json(DeviceTrustResponse {
        lease: device_response_from_row(row),
    }))
}

pub(super) async fn session_device_events_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let user = match state.auth_service.current_user(&headers).await {
        Ok(user) => user,
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };
    if let Err((status, error)) = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await
    {
        return sse_error_response(status, error.0.detail);
    }
    let Some(pool) = state.shared_pool.as_ref() else {
        return sse_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "shared MatrixOne pool is not configured",
        );
    };
    let buffered = match load_device_lease_event_payloads(pool, &user.user_id, &session_id).await {
        Ok(events) => events
            .into_iter()
            .filter_map(|event| serde_json::to_value(event).ok())
            .collect::<Vec<_>>(),
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };
    let mut rx = super::device_lease_sweeper::subscribe_device_lease_events();
    let stream = async_stream::stream! {
        for event in buffered {
            yield Ok::<_, std::convert::Infallible>(format!(
                "data: {}\n\n",
                serde_json::to_string(&event).unwrap_or_default()
            ));
        }
        while let Ok(event) = rx.recv().await {
            if event.get("session_id").and_then(serde_json::Value::as_str) != Some(session_id.as_str()) {
                continue;
            }
            yield Ok::<_, std::convert::Infallible>(format!(
                "data: {}\n\n",
                serde_json::to_string(&event).unwrap_or_default()
            ));
        }
    };
    bridge::sse_stream_response(StatusCode::OK, Body::from_stream(stream))
}

pub(super) async fn delete_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .session_service
        .delete_session(session_id, user.user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn close_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some("closed".to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn resume_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some("active".to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn cancel_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some(STATUS_CANCELLED.to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn session_activity_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionActivityQuery>,
) -> Result<Json<SessionActivityResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    // Verify ownership first.
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;

    let activities = state
        .session_service
        .get_session_activity(session_id, user.user_id, query.limit, query.offset)
        .await?;
    Ok(Json(SessionActivityResponse::from(activities)))
}

pub(super) async fn list_session_artifacts_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionArtifactListQuery>,
) -> Result<Json<SessionArtifactListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifacts = artifact_store
        .list_json_artifacts(
            &session_id,
            query.artifact_kind.as_deref(),
            query.limit as usize,
        )
        .await
        .map_err(internal_artifact_error)?;
    Ok(Json(SessionArtifactListResponse {
        session_id,
        artifacts: artifacts
            .into_iter()
            .map(session_artifact_response)
            .collect(),
        limit: query.limit,
    }))
}

pub(super) async fn get_latest_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_kind)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<SessionArtifactResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifact = artifact_store
        .load_latest_json_artifact(&session_id, &artifact_kind)
        .await
        .map_err(internal_artifact_error)?
        .ok_or_else(session_artifact_not_found)?;
    Ok(Json(session_artifact_response(artifact)))
}

pub(super) async fn get_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<SessionArtifactResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifact = artifact_store
        .load_json_artifact(&artifact_id)
        .await
        .map_err(internal_artifact_error)?
        .filter(|artifact| artifact.session_id == session_id)
        .ok_or_else(session_artifact_not_found)?;
    Ok(Json(session_artifact_response(artifact)))
}

pub(super) async fn download_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<PresignedArtifactDownload>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id.clone();
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_artifact_error("shared MatrixOne pool is not configured"))?;
    let row = sqlx::query(
        "SELECT artifact_id, status, cold_storage_ref
         FROM session_artifacts
         WHERE artifact_id = ? AND session_id = ? AND user_id = ?
         LIMIT 1",
    )
    .bind(&artifact_id)
    .bind(&session_id)
    .bind(&user_id)
    .fetch_optional(pool.get())
    .await
    .map_err(internal_artifact_error)?
    .ok_or_else(session_artifact_not_found)?;
    let status = row.try_get::<String, _>("status").unwrap_or_default();
    if status == "expired" {
        return Err((
            StatusCode::GONE,
            Json(ErrorResponse::new(
                "artifact raw payload has expired; summary remains available",
            )),
        ));
    }
    let base_path = format!(
        "/sessions/{}/artifacts/{}/download/presigned",
        session_id, artifact_id
    );
    Ok(Json(build_presigned_artifact_download(
        &base_path,
        &user_id,
        &session_id,
        &artifact_id,
        &state.chat_turn_bridge_secret,
        Utc::now(),
        300,
    )))
}

fn session_artifact_store(
    state: &AppState,
) -> Result<DatabaseSessionArtifactStore, (StatusCode, Json<ErrorResponse>)> {
    let Some(shared_pool) = state.shared_pool.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("session artifact store unavailable")),
        ));
    };
    Ok(DatabaseSessionArtifactStore::new(shared_pool.settings().clone()).with_pool(shared_pool))
}

fn session_artifact_response(
    artifact: astra_services::StoredSessionArtifact,
) -> SessionArtifactResponse {
    SessionArtifactResponse {
        artifact_id: artifact.artifact_id,
        session_id: artifact.session_id,
        user_id: artifact.user_id,
        artifact_kind: artifact.artifact_kind,
        source: artifact.source,
        turn: artifact.turn,
        round: artifact.round,
        content: artifact.content,
        metadata: artifact.metadata,
        retention_policy: artifact.retention_policy,
        retention_until: artifact.retention_until,
        status: artifact.status,
        referenced_by_manifest_count: artifact.referenced_by_manifest_count,
        referenced_by_state_items_count: artifact.referenced_by_state_items_count,
        referenced_by_citation_count: artifact.referenced_by_citation_count,
        created_at: artifact.created_at,
    }
}

fn session_artifact_not_found() -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("session artifact not found")),
    )
}

fn internal_artifact_error(error: impl std::fmt::Display) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(error.to_string())),
    )
}

#[derive(Clone, Debug)]
struct DeviceLeaseRow {
    lease_id: String,
    user_id: String,
    session_id: String,
    device_id: String,
    device_fingerprint: String,
    status: String,
}

struct SessionStateRevisionWrite<'a> {
    session_id: &'a str,
    user_id: &'a str,
    monotonic_id: i64,
    revision_hash: &'a str,
    device_fingerprint: &'a str,
    transcript_high_watermark: i64,
    run_event_high_watermark: i64,
    state_projection_hash: &'a str,
}

async fn persist_session_state_revision(
    pool: &SharedPool,
    revision: SessionStateRevisionWrite<'_>,
) -> Result<(), sqlx::Error> {
    if update_session_state_revision(pool, &revision).await? {
        return Ok(());
    }

    let insert_result = sqlx::query(
        "INSERT INTO session_state_revisions
         (session_id, user_id, monotonic_id, revision_hash, device_fingerprint,
          transcript_high_watermark, run_event_high_watermark, state_projection_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
    )
    .bind(revision.session_id)
    .bind(revision.user_id)
    .bind(revision.monotonic_id)
    .bind(revision.revision_hash)
    .bind(revision.device_fingerprint)
    .bind(revision.transcript_high_watermark)
    .bind(revision.run_event_high_watermark)
    .bind(revision.state_projection_hash)
    .execute(pool.get())
    .await;

    match insert_result {
        Ok(_) => Ok(()),
        Err(error) if is_duplicate_key_error(&error) => {
            update_session_state_revision(pool, &revision).await?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn update_session_state_revision(
    pool: &SharedPool,
    revision: &SessionStateRevisionWrite<'_>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE session_state_revisions
         SET user_id = ?,
             monotonic_id = ?,
             revision_hash = ?,
             device_fingerprint = ?,
             transcript_high_watermark = ?,
             run_event_high_watermark = ?,
             state_projection_hash = ?,
             updated_at = NOW(6)
         WHERE session_id = ?",
    )
    .bind(revision.user_id)
    .bind(revision.monotonic_id)
    .bind(revision.revision_hash)
    .bind(revision.device_fingerprint)
    .bind(revision.transcript_high_watermark)
    .bind(revision.run_event_high_watermark)
    .bind(revision.state_projection_hash)
    .bind(revision.session_id)
    .execute(pool.get())
    .await?;
    Ok(result.rows_affected() > 0)
}

async fn ensure_device_lease(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    device_id: &str,
    device_fingerprint: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(DEVICE_LEASE_TTL_HOURS);
    let updated = refresh_device_lease(
        pool,
        user_id,
        session_id,
        device_id,
        device_fingerprint,
        expires_at,
    )
    .await
    .map_err(|error| {
        internal_error(format!(
            "refresh device lease failed for session {session_id} device {device_id}: {error}"
        ))
    })?;
    if updated {
        return Ok(());
    }

    let insert_result = sqlx::query(
        "INSERT INTO session_device_leases
         (lease_id, user_id, session_id, device_id, device_fingerprint, trust_level,
          status, last_monotonic_id, expires_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'new_device', 'active', 0, ?, NOW(6), NOW(6))",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(session_id)
    .bind(device_id)
    .bind(device_fingerprint)
    .bind(expires_at.naive_utc())
    .execute(pool.get())
    .await;

    match insert_result {
        Ok(_) => Ok(()),
        Err(error) if is_duplicate_key_error(&error) => {
            refresh_device_lease(
                pool,
                user_id,
                session_id,
                device_id,
                device_fingerprint,
                expires_at,
            )
            .await
            .map_err(|source| {
                internal_error(format!(
                    "refresh device lease after duplicate failed for session {session_id} device {device_id}: {source}"
                ))
            })?;
            Ok(())
        }
        Err(error) => Err(internal_error(format!(
            "insert device lease failed for session {session_id} device {device_id}: {error}"
        ))),
    }
}

async fn refresh_device_lease(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    device_id: &str,
    device_fingerprint: &str,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE session_device_leases
         SET device_fingerprint = ?, status = 'active', expires_at = ?, revoked_at = NULL, updated_at = NOW(6)
         WHERE session_id = ? AND device_id = ? AND user_id = ?",
    )
    .bind(device_fingerprint)
    .bind(expires_at.naive_utc())
    .bind(session_id)
    .bind(device_id)
    .bind(user_id)
    .execute(pool.get())
    .await?;
    Ok(result.rows_affected() > 0)
}

async fn transcript_high_watermark(
    pool: &SharedPool,
    session_id: &str,
) -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT COALESCE(MAX(item_seq), 0) AS high_watermark
         FROM session_transcript_items
         WHERE session_id = ?",
    )
    .bind(session_id)
    .fetch_one(pool.get())
    .await
    .map_err(internal_error)?;
    Ok(row.try_get::<i64, _>("high_watermark").unwrap_or(0))
}

async fn active_run_projection(
    pool: &SharedPool,
    session_id: &str,
) -> Result<Option<ActiveRunProjection>, (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT run_id, last_event_idx
         FROM agent_runs
         WHERE session_id = ? AND status IN ('running', 'waiting', 'paused')
         ORDER BY updated_at DESC
         LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool.get())
    .await
    .map_err(internal_error)?;
    Ok(row.map(|row| ActiveRunProjection {
        run_id: row.try_get("run_id").unwrap_or_default(),
        run_event_high_watermark: row.try_get::<i64, _>("last_event_idx").unwrap_or(0).max(0),
        replay_required: false,
        replay_start_event_idx: 0,
    }))
}

fn state_monotonic_id(
    known: u64,
    transcript_high_watermark: i64,
    run_event_high_watermark: i64,
) -> u64 {
    known.max(
        transcript_high_watermark
            .max(run_event_high_watermark)
            .max(0) as u64,
    )
}

fn revision_hash(
    session_id: &str,
    monotonic_id: u64,
    device_fingerprint: &str,
    transcript_high_watermark: i64,
    run_event_high_watermark: i64,
    state_projection_hash: &str,
) -> String {
    sha256_hex(
        format!(
            "{session_id}|{monotonic_id}|{device_fingerprint}|{transcript_high_watermark}|{run_event_high_watermark}|{state_projection_hash}"
        )
        .as_bytes(),
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn device_response_from_row(row: sqlx::mysql::MySqlRow) -> DeviceLeaseResponse {
    DeviceLeaseResponse {
        lease_id: row.try_get("lease_id").unwrap_or_default(),
        session_id: row.try_get("session_id").unwrap_or_default(),
        device_id: row.try_get("device_id").unwrap_or_default(),
        device_fingerprint: row.try_get("device_fingerprint").unwrap_or_default(),
        trust_level: row.try_get("trust_level").unwrap_or_default(),
        status: row.try_get("status").unwrap_or_default(),
        last_monotonic_id: row.try_get("last_monotonic_id").unwrap_or_default(),
        expires_at: row.try_get("expires_at").unwrap_or_default(),
    }
}

async fn load_device_lease_for_revoke(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    lease_id: Option<&str>,
    device_id: Option<&str>,
) -> Result<DeviceLeaseRow, (StatusCode, Json<ErrorResponse>)> {
    let row = if let Some(lease_id) = lease_id {
        sqlx::query(
            "SELECT lease_id, user_id, session_id, device_id, device_fingerprint, status
             FROM session_device_leases
             WHERE lease_id = ? AND session_id = ? AND user_id = ?
             LIMIT 1",
        )
        .bind(lease_id)
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(pool.get())
        .await
        .map_err(internal_error)?
    } else if let Some(device_id) = device_id {
        sqlx::query(
            "SELECT lease_id, user_id, session_id, device_id, device_fingerprint, status
             FROM session_device_leases
             WHERE device_id = ? AND session_id = ? AND user_id = ?
             LIMIT 1",
        )
        .bind(device_id)
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(pool.get())
        .await
        .map_err(internal_error)?
    } else {
        return Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "lease_id or device_id is required",
        ));
    };

    let Some(row) = row else {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "device lease not found",
        ));
    };
    Ok(DeviceLeaseRow {
        lease_id: row.try_get("lease_id").unwrap_or_default(),
        user_id: row.try_get("user_id").unwrap_or_default(),
        session_id: row.try_get("session_id").unwrap_or_default(),
        device_id: row.try_get("device_id").unwrap_or_default(),
        device_fingerprint: row.try_get("device_fingerprint").unwrap_or_default(),
        status: row.try_get("status").unwrap_or_default(),
    })
}

async fn insert_device_lease_event(
    pool: &SharedPool,
    lease: &DeviceLeaseRow,
    event_type: &str,
    reason: &str,
) -> Result<DeviceLeaseEndedPayload, (StatusCode, Json<ErrorResponse>)> {
    let ended_at_server = chrono::Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO session_device_lease_events
         (lease_event_id, lease_id, user_id, session_id, device_id, device_fingerprint,
          event_type, reason, ended_at_server, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&lease.lease_id)
    .bind(&lease.user_id)
    .bind(&lease.session_id)
    .bind(&lease.device_id)
    .bind(&lease.device_fingerprint)
    .bind(event_type)
    .bind(reason)
    .bind(ended_at_server)
    .execute(pool.get())
    .await
    .map_err(internal_error)?;
    Ok(DeviceLeaseEndedPayload {
        r#type: event_type.to_string(),
        lease_id: lease.lease_id.clone(),
        session_id: lease.session_id.clone(),
        device_id: lease.device_id.clone(),
        device_fingerprint: lease.device_fingerprint.clone(),
        reason: reason.to_string(),
        ended_at_server: ended_at_server.format("%Y-%m-%dT%H:%M:%S").to_string(),
    })
}

async fn load_device_lease_event_payloads(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
) -> Result<Vec<DeviceLeaseEndedPayload>, (StatusCode, Json<ErrorResponse>)> {
    let rows = sqlx::query(
        "SELECT lease_id, session_id, device_id, device_fingerprint, event_type, reason,
                DATE_FORMAT(ended_at_server, '%Y-%m-%dT%H:%i:%s') AS ended_at_server
         FROM session_device_lease_events
         WHERE user_id = ? AND session_id = ?
         ORDER BY created_at ASC
         LIMIT 200",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_all(pool.get())
    .await
    .map_err(internal_error)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let event_type = row.try_get::<String, _>("event_type").unwrap_or_default();
            DeviceLeaseEndedPayload {
                r#type: if event_type == "auto_expire" {
                    "device_lease_expired".to_string()
                } else {
                    event_type
                },
                lease_id: row.try_get("lease_id").unwrap_or_default(),
                session_id: row.try_get("session_id").unwrap_or_default(),
                device_id: row.try_get("device_id").unwrap_or_default(),
                device_fingerprint: row.try_get("device_fingerprint").unwrap_or_default(),
                reason: row.try_get("reason").unwrap_or_default(),
                ended_at_server: row.try_get("ended_at_server").unwrap_or_default(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use astra_core::{ErrorResponse, error_response};
    use astra_services::auth::SessionActivityRecord;
    use astra_services::{
        AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
        AuthTokenRecord, AuthUserRecord, SessionCreateRequestData, SessionListFilter,
        SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
    };
    use async_trait::async_trait;
    use axum::{
        Json,
        extract::{Path, Query, State},
        http::{HeaderMap, HeaderValue, StatusCode},
    };
    use serde_json::json;
    use tokio::sync::Mutex;

    use crate::{AppState, HealthChecker, ServiceInfo};

    #[derive(Clone)]
    struct AlwaysHealthy;

    #[async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct RecordingAuthService;

    #[async_trait]
    impl AuthService for RecordingAuthService {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("register is not used in session handler tests")
        }

        async fn login(
            &self,
            _request: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("login is not used in session handler tests")
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("refresh is not used in session handler tests")
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!("logout is not used in session handler tests")
        }

        async fn current_user(
            &self,
            headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            if headers.get("authorization").is_none() {
                return Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing authorization",
                ));
            }
            Ok(AuthUserRecord {
                user_id: "artifact-owner".into(),
                username: "artifact-owner".into(),
                email: "artifact@example.com".into(),
                display_name: None,
            })
        }
    }

    #[derive(Default)]
    struct RecordingSessionService {
        get_session_calls: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl SessionService for RecordingSessionService {
        async fn create_session(
            &self,
            _user_id: String,
            _request: SessionCreateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("create_session is not used in session artifact tests")
        }

        async fn list_sessions(
            &self,
            _filter: SessionListFilter,
        ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("list_sessions is not used in session artifact tests")
        }

        async fn get_session(
            &self,
            session_id: String,
            user_id: String,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            self.get_session_calls
                .lock()
                .await
                .push((session_id, user_id));
            Err(error_response(
                StatusCode::FORBIDDEN,
                "session access denied",
            ))
        }

        async fn update_session(
            &self,
            _session_id: String,
            _user_id: String,
            _request: SessionUpdateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("update_session is not used in session artifact tests")
        }

        async fn delete_session(
            &self,
            _session_id: String,
            _user_id: String,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!("delete_session is not used in session artifact tests")
        }

        async fn get_session_activity(
            &self,
            _session_id: String,
            _user_id: String,
            _limit: u32,
            _offset: u32,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("get_session_activity is not used in session artifact tests")
        }
    }

    fn build_state(
        auth_service: Arc<dyn AuthService>,
        session_service: Arc<dyn SessionService>,
    ) -> AppState {
        AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
            .with_auth_service(auth_service)
            .with_session_service(session_service)
    }

    fn auth_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer session-test-token"),
        );
        headers
    }

    #[tokio::test]
    async fn session_artifact_handlers_verify_session_ownership_before_store_access() {
        let session_service = Arc::new(RecordingSessionService::default());
        let state = build_state(Arc::new(RecordingAuthService), session_service.clone());
        let session_id = "session-123".to_string();

        let list_err = match list_session_artifacts_handler(
            State(state.clone()),
            Path(session_id.clone()),
            auth_headers(),
            Query(SessionArtifactListQuery::default()),
        )
        .await
        {
            Ok(_) => panic!("list should stop at session ownership check"),
            Err(err) => err,
        };
        assert_eq!(list_err.0, StatusCode::FORBIDDEN);

        let latest_err = match get_latest_session_artifact_handler(
            State(state.clone()),
            Path((session_id.clone(), "trace".to_string())),
            auth_headers(),
        )
        .await
        {
            Ok(_) => panic!("latest should stop at session ownership check"),
            Err(err) => err,
        };
        assert_eq!(latest_err.0, StatusCode::FORBIDDEN);

        let get_err = match get_session_artifact_handler(
            State(state.clone()),
            Path((session_id.clone(), "artifact-1".to_string())),
            auth_headers(),
        )
        .await
        {
            Ok(_) => panic!("get should stop at session ownership check"),
            Err(err) => err,
        };
        assert_eq!(get_err.0, StatusCode::FORBIDDEN);

        let download_err = match download_session_artifact_handler(
            State(state),
            Path((session_id.clone(), "artifact-1".to_string())),
            auth_headers(),
        )
        .await
        {
            Ok(_) => panic!("download should stop at session ownership check"),
            Err(err) => err,
        };
        assert_eq!(download_err.0, StatusCode::FORBIDDEN);

        let calls = session_service.get_session_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![
                (session_id.clone(), "artifact-owner".to_string()),
                (session_id.clone(), "artifact-owner".to_string()),
                (session_id.clone(), "artifact-owner".to_string()),
                (session_id, "artifact-owner".to_string()),
            ],
            "artifact handlers should verify session ownership before touching artifact storage"
        );
    }

    #[test]
    fn reasoning_projection_collects_sse_reasoning_events() {
        let mut projection = TranscriptReasoningProjection::default();
        apply_reasoning_event_payload(
            &mut projection,
            &json!({"type": "reasoning_delta", "content": "checking "}),
        );
        apply_reasoning_event_payload(
            &mut projection,
            &json!({"type": "reasoning_delta", "content": "context"}),
        );
        apply_reasoning_event_payload(&mut projection, &json!({"type": "reasoning_done"}));

        assert_eq!(
            projection.reasoning().as_deref(),
            Some("checking context"),
            "transcript hydration should reconstruct persisted reasoning deltas in event order"
        );
        assert_eq!(
            projection.status().as_deref(),
            Some("complete"),
            "hydrated reasoning blocks should render as complete after persistence"
        );
    }

    #[test]
    fn reasoning_projection_reads_event_type_and_nested_content() {
        let mut projection = TranscriptReasoningProjection::default();
        apply_reasoning_event_payload(
            &mut projection,
            &json!({"event_type": "reasoning_message_content", "data": {"content": "nested reasoning"}}),
        );
        apply_reasoning_event_payload(
            &mut projection,
            &json!({"event_type": "thinking_done", "data": {}}),
        );

        assert_eq!(
            projection.reasoning().as_deref(),
            Some("nested reasoning"),
            "reasoning hydration should support persisted event_type/data.content payloads"
        );
        assert!(
            projection.done,
            "thinking_done should mark the block complete"
        );
    }
}
