use crate::db_row::RowExt;
use crate::server::run::lifecycle::TranscriptPersistPayload;
use crate::server::*;
use astra_core::{STATUS_CANCELLED, error_response, is_duplicate_key_error};
use astra_services::context_manifest::session_artifact_raw_payload_is_available;
use astra_services::session_restore::SessionRestoreService;
use astra_services::session_workspace::{WORKSPACE_METADATA_ARTIFACT_KIND, WorkspaceMetadata};
use astra_services::{
    DatabaseSessionArtifactStore, DatabaseStateProjectionStore, PresignedArtifactDownload,
    SessionArtifactJsonStore, StoredSessionArtifact, UserAnchorMemoryItem,
    build_presigned_artifact_download,
};
use astra_thin_client::device_proof::{DeviceProofPurpose, canonical_device_proof_message};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

const DEFAULT_TRANSCRIPT_LIMIT: u32 = 50;
const MAX_TRANSCRIPT_LIMIT: u32 = 200;
const DEFAULT_RESUMABLE_SESSION_LIMIT: u32 = 20;
const MAX_RESUMABLE_SESSION_LIMIT: u32 = 50;
const DEVICE_LEASE_TTL_HOURS: i64 = 2;
const DEVICE_CHALLENGE_TTL_SECONDS: i64 = 120;
const DEVICE_PROTOCOL_VALUE_MAX_LEN: usize = 128;
const DEVICE_PROOF_MAX_LEN: usize = 128;
const DEVICE_ID_HEADER: &str = "x-astra-device-id";
const DEVICE_FINGERPRINT_HEADER: &str = "x-astra-device-fingerprint";
const DEVICE_CHALLENGE_ID_HEADER: &str = "x-astra-device-challenge-id";
const DEVICE_PROOF_HEADER: &str = "x-astra-device-proof";
type HmacSha256 = Hmac<Sha256>;

#[derive(Deserialize, Default)]
pub(crate) struct ResumableSessionsQuery {
    #[serde(default = "default_resumable_session_limit")]
    pub limit: u32,
}

fn default_resumable_session_limit() -> u32 {
    DEFAULT_RESUMABLE_SESSION_LIMIT
}

#[derive(Deserialize, Default)]
pub(crate) struct SessionStateQuery {
    #[serde(default)]
    pub known_state_revision: u64,
    #[serde(default)]
    pub known_revision_hash: Option<String>,
    #[serde(default)]
    pub client_cache_empty: bool,
}

#[derive(Serialize)]
pub(crate) struct SessionStateResponse {
    pub session_id: String,
    pub state_revision: StateRevisionResponse,
    pub transcript_high_watermark: i64,
    pub active_run: Option<ActiveRunProjection>,
    pub workspace_authority: Option<WorkspaceAuthorityResponse>,
    pub latest_context_manifest: Option<ContextManifestSummaryResponse>,
    pub state_summary: Vec<StateCategorySummaryResponse>,
    pub artifact_previews: Vec<ArtifactPreviewResponse>,
    pub anchor_memory: Vec<UserAnchorMemoryResponse>,
    pub projection_observability: SessionProjectionObservabilityResponse,
    pub replay_required: bool,
    pub transcript_replay_required: bool,
    pub run_event_replay_required: bool,
}

#[derive(Serialize)]
pub(crate) struct StateRevisionResponse {
    pub monotonic_id: u64,
    pub revision_hash: String,
}

#[derive(Serialize)]
pub(crate) struct ActiveRunProjection {
    pub run_id: String,
    pub run_event_high_watermark: i64,
    pub replay_required: bool,
    pub replay_start_event_idx: i64,
}

#[derive(Serialize)]
pub(crate) struct UserAnchorMemoryResponse {
    pub item_id: String,
    pub category: String,
    pub item_key: String,
    pub summary_text: Option<String>,
    pub token_estimate: u32,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceAuthorityResponse {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub updated_at: String,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct ContextManifestSummaryResponse {
    pub manifest_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub turn_id: String,
    pub reason: String,
    pub total_estimated_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_template_id: Option<String>,
    pub policy_version: String,
    pub created_at: String,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct StateCategorySummaryResponse {
    pub category: String,
    pub count: u32,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct ArtifactPreviewResponse {
    pub artifact_id: String,
    pub artifact_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct PromptRequestObservabilityResponse {
    pub request_id: String,
    pub request_hash: String,
    pub message_count: u32,
    pub tool_count: u32,
    pub delta_counts: astra_services::PromptDeltaCounts,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct SessionProjectionObservabilityResponse {
    pub observability_available: bool,
    pub transcript_page_count: u32,
    pub transcript_page_high_watermark: i64,
    pub transcript_page_lag_items: i64,
    pub active_run_projection_lag_events: i64,
    pub prompt_request_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_prompt_request: Option<PromptRequestObservabilityResponse>,
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
pub(crate) struct TranscriptQuery {
    pub before_seq: Option<i64>,
    pub run_id: Option<String>,
    pub scope: Option<TranscriptQueryScope>,
    #[serde(default = "default_transcript_limit")]
    pub limit: u32,
}

/// The server owns the scope decision for durable transcript reads. A client
/// cannot reconstruct a main conversation by filtering rows after it has
/// already received child-agent content.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranscriptQueryScope {
    RootConversation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptReadScope<'a> {
    Session,
    Run(&'a str),
    RootConversation,
}

fn transcript_read_scope(
    query: &TranscriptQuery,
) -> Result<TranscriptReadScope<'_>, (StatusCode, Json<ErrorResponse>)> {
    match (query.scope, query.run_id.as_deref()) {
        (Some(TranscriptQueryScope::RootConversation), Some(_)) => Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "transcript scope=root_conversation cannot be combined with run_id",
        )),
        (Some(TranscriptQueryScope::RootConversation), None) => {
            Ok(TranscriptReadScope::RootConversation)
        }
        (None, Some(run_id)) => Ok(TranscriptReadScope::Run(run_id)),
        (None, None) => Ok(TranscriptReadScope::Session),
    }
}

#[derive(Serialize)]
pub(crate) struct TranscriptResponse {
    pub session_id: String,
    pub items: Vec<TranscriptItemResponse>,
    pub page_refs: Vec<TranscriptPageRefResponse>,
    pub next_before_seq: Option<i64>,
    pub has_more: bool,
}

#[derive(Serialize)]
pub(crate) struct TranscriptPageRefResponse {
    pub page_seq: i64,
    pub start_item_seq: i64,
    pub end_item_seq: i64,
    pub item_count: i64,
    pub page_hash: String,
}

#[derive(Serialize)]
pub(crate) struct TranscriptItemResponse {
    pub session_id: String,
    pub item_seq: i64,
    pub run_id: Option<String>,
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_status: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<astra_thin_client::SessionTranscriptToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<astra_thin_client::SessionTranscriptToolResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<astra_turn_types::AgentTranscriptEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<String>,
    pub created_at: String,
}

fn session_row_string(row: &impl RowExt, column: &str) -> Result<String, String> {
    row.string_column(column)
        .map_err(|error| format!("session row decode column `{column}`: {error}"))
}

fn session_row_optional_string(row: &impl RowExt, column: &str) -> Result<Option<String>, String> {
    row.optional_string_column(column)
        .map_err(|error| format!("session row decode column `{column}`: {error}"))
}

fn session_row_i64(row: &impl RowExt, column: &str) -> Result<i64, String> {
    row.i64_column(column)
        .map_err(|error| format!("session row decode column `{column}`: {error}"))
}

fn session_row_non_negative_i64(row: &impl RowExt, column: &'static str) -> Result<i64, String> {
    let value = session_row_i64(row, column)?;
    if value < 0 {
        return Err(format!(
            "session row decode column `{column}` expected non-negative integer, got {value}"
        ));
    }
    Ok(value)
}

fn session_row_u32(row: &impl RowExt, column: &'static str) -> Result<u32, String> {
    let value = session_row_i64(row, column)?;
    u32::try_from(value).map_err(|_| {
        format!("session row decode column `{column}` expected u32 range, got {value}")
    })
}

#[derive(Serialize)]
pub(crate) struct DeviceLeaseResponse {
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
pub(crate) struct DeviceListResponse {
    pub session_id: String,
    pub devices: Vec<DeviceLeaseResponse>,
}

#[derive(Deserialize, Default)]
pub(crate) struct DeviceRevokeRequest {
    pub lease_id: Option<String>,
    pub device_id: Option<String>,
    #[serde(default)]
    pub expected_last_monotonic_id: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceTrustRequest {
    pub device_id: String,
    pub device_fingerprint: String,
    pub challenge_id: String,
    pub device_proof: String,
    pub reauthentication_proof: String,
    #[serde(default)]
    pub expected_last_monotonic_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceEnrollRequest {
    pub device_id: String,
    pub device_fingerprint: String,
    #[serde(default)]
    pub reauthentication_proof: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct DeviceEnrollResponse {
    pub lease: DeviceLeaseResponse,
    pub device_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceChallengeRequest {
    pub device_id: String,
    pub device_fingerprint: String,
    pub purpose: DeviceProofPurpose,
}

#[derive(Serialize)]
pub(crate) struct DeviceChallengeResponse {
    pub challenge_id: String,
    pub challenge: String,
    pub purpose: DeviceProofPurpose,
    pub expires_in: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VerifiedDeviceIdentity {
    device_id: String,
    device_fingerprint: String,
}

#[derive(Serialize)]
pub(crate) struct DeviceRevokeResponse {
    pub event: DeviceLeaseEndedPayload,
    pub idempotent: bool,
}

#[derive(Serialize)]
pub(crate) struct DeviceTrustResponse {
    pub lease: DeviceLeaseResponse,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DeviceLeaseEndedPayload {
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

pub(crate) async fn create_session_handler(
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

pub(crate) async fn list_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<SessionListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let cursor = query.cursor()?;
    let sessions = state
        .session_service
        .list_sessions(SessionListFilter {
            user_id: user.user_id,
            agent_id: query.agent_id,
            status: query.session_status,
            limit: query.limit,
            cursor,
        })
        .await?;
    Ok(Json(SessionListResponse::from(sessions)))
}

pub(crate) async fn list_resumable_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ResumableSessionsQuery>,
) -> Result<
    Json<astra_services::session_restore::ResumableSessionsResponse>,
    (StatusCode, Json<ErrorResponse>),
> {
    let user = state.auth_service.current_user(&headers).await?;
    let Some(shared_pool) = state.shared_pool.as_ref() else {
        return Err(internal_error("shared MatrixOne pool is not configured"));
    };
    let limit = query.limit.clamp(1, MAX_RESUMABLE_SESSION_LIMIT);
    let svc = astra_services::session_restore::HybridRestoreService::new(shared_pool.get().clone());
    let mut sessions = svc
        .list_resumable_sessions(&user.user_id)
        .await
        .map_err(internal_error)?;
    if sessions.len() > limit as usize {
        sessions.truncate(limit as usize);
    }
    Ok(Json(
        astra_services::session_restore::ResumableSessionsResponse { sessions },
    ))
}

pub(crate) async fn get_session_handler(
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

pub(crate) async fn get_session_state_handler(
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
    let device = verify_device_proof_from_headers(
        pool,
        &session.user_id,
        &session.session_id,
        &headers,
        DeviceProofPurpose::Hydrate,
    )
    .await?;
    let device_fingerprint = device.device_fingerprint;

    let transcript_high_watermark =
        transcript_high_watermark(pool, &session.user_id, &session.session_id).await?;
    let active_run = active_run_projection(pool, &session.user_id, &session.session_id).await?;
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
    let workspace_authority =
        load_workspace_authority(&state, &session.user_id, &session.session_id).await?;
    let latest_context_manifest = load_latest_context_manifest(
        pool,
        &session.user_id,
        &session.session_id,
        active_run.as_ref().map(|run| run.run_id.as_str()),
    )
    .await?;
    let state_summary = load_state_summary(pool, &session.user_id, &session.session_id).await?;
    let artifact_previews =
        load_artifact_previews(&state, &session.user_id, &session.session_id).await?;
    let projection_observability = load_session_projection_observability(
        pool,
        &session.user_id,
        &session.session_id,
        transcript_high_watermark,
        active_run.as_ref(),
    )
    .await?;
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
        workspace_authority,
        latest_context_manifest,
        state_summary,
        artifact_previews,
        anchor_memory,
        projection_observability,
        replay_required,
        transcript_replay_required,
        run_event_replay_required,
    }))
}

pub(crate) async fn get_session_transcript_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<TranscriptQuery>,
) -> Result<Json<TranscriptResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id.clone();
    let _ = state
        .session_service
        .get_session(session_id.clone(), user_id.clone())
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
    let scope = transcript_read_scope(&query)?;
    let limit = query.limit.clamp(1, MAX_TRANSCRIPT_LIMIT);
    let before_seq = query.before_seq.unwrap_or(i64::MAX);
    let rows = match scope {
        TranscriptReadScope::Run(run_id) => sqlx::query(
            "SELECT session_id, item_seq, run_id, role, content, payload_json, source_event_id,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at
             FROM session_transcript_items
             WHERE session_id = ? AND user_id = ? AND run_id = ? AND item_seq < ?
             ORDER BY item_seq DESC
             LIMIT ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(run_id)
        .bind(before_seq)
        .bind(i64::from(limit))
        .fetch_all(pool.get())
        .await,
        TranscriptReadScope::Session => sqlx::query(
            "SELECT session_id, item_seq, run_id, role, content, payload_json, source_event_id,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at
             FROM session_transcript_items
             WHERE session_id = ? AND user_id = ? AND item_seq < ?
             ORDER BY item_seq DESC
             LIMIT ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(before_seq)
        .bind(i64::from(limit))
        .fetch_all(pool.get())
        .await,
        TranscriptReadScope::RootConversation => sqlx::query(
            "SELECT session_transcript_items.session_id AS session_id,
                    session_transcript_items.item_seq AS item_seq,
                    session_transcript_items.run_id AS run_id,
                    session_transcript_items.role AS role,
                    session_transcript_items.content AS content,
                    session_transcript_items.payload_json AS payload_json,
                    session_transcript_items.source_event_id AS source_event_id,
                    DATE_FORMAT(session_transcript_items.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at
             FROM session_transcript_items
             LEFT JOIN agent_runs ON agent_runs.user_id = session_transcript_items.user_id
                 AND agent_runs.session_id = session_transcript_items.session_id
                 AND agent_runs.run_id = session_transcript_items.run_id
             WHERE session_transcript_items.session_id = ?
                 AND session_transcript_items.user_id = ?
                 AND (session_transcript_items.run_id IS NULL
                     OR (agent_runs.run_id IS NOT NULL AND agent_runs.parent_run_id IS NULL))
                 AND session_transcript_items.item_seq < ?
             ORDER BY session_transcript_items.item_seq DESC
             LIMIT ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(before_seq)
        .bind(i64::from(limit))
        .fetch_all(pool.get())
        .await,
    }
    .map_err(internal_error)?;

    let mut items = rows
        .into_iter()
        .map(|row| decode_transcript_item(&row))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            internal_error(format!(
                "decode transcript item failed for session {session_id}: {error}"
            ))
        })?;
    items.reverse();
    let transcript_item_count = items.len();
    let transcript_first_seq = items.first().map(|item| item.item_seq);
    let transcript_last_seq = items.last().map(|item| item.item_seq);
    let page_refs = if matches!(scope, TranscriptReadScope::RootConversation) {
        // Physical page hashes cover the whole session stream. Returning one
        // for a filtered root projection would incorrectly claim that it
        // hashes only the rows in this response.
        Vec::new()
    } else {
        load_transcript_page_refs(
            pool,
            &user_id,
            &session_id,
            transcript_first_seq,
            transcript_last_seq,
        )
        .await
        .map_err(|error| {
            internal_error(format!(
                "load transcript page refs failed for session {session_id}: {error}"
            ))
        })?
    };
    let next_before_seq = transcript_first_seq;
    let has_more = if transcript_item_count == limit as usize {
        match (scope, next_before_seq) {
            (TranscriptReadScope::Run(run_id), Some(next_before_seq)) => sqlx::query(
                "SELECT 1 AS older
                 FROM session_transcript_items
                 WHERE session_id = ? AND user_id = ? AND run_id = ? AND item_seq < ?
                 LIMIT 1",
            )
            .bind(&session_id)
            .bind(&user_id)
            .bind(run_id)
            .bind(next_before_seq)
            .fetch_optional(pool.get())
            .await
            .map_err(internal_error)?
            .is_some(),
            (TranscriptReadScope::Session, Some(next_before_seq)) => sqlx::query(
                "SELECT 1 AS older
                 FROM session_transcript_items
                 WHERE session_id = ? AND user_id = ? AND item_seq < ?
                 LIMIT 1",
            )
            .bind(&session_id)
            .bind(&user_id)
            .bind(next_before_seq)
            .fetch_optional(pool.get())
            .await
            .map_err(internal_error)?
            .is_some(),
            (TranscriptReadScope::RootConversation, Some(next_before_seq)) => sqlx::query(
                "SELECT 1 AS older
                 FROM session_transcript_items
                 LEFT JOIN agent_runs ON agent_runs.user_id = session_transcript_items.user_id
                     AND agent_runs.session_id = session_transcript_items.session_id
                     AND agent_runs.run_id = session_transcript_items.run_id
                 WHERE session_transcript_items.session_id = ?
                     AND session_transcript_items.user_id = ?
                     AND (session_transcript_items.run_id IS NULL
                         OR (agent_runs.run_id IS NOT NULL AND agent_runs.parent_run_id IS NULL))
                     AND session_transcript_items.item_seq < ?
                 LIMIT 1",
            )
            .bind(&session_id)
            .bind(&user_id)
            .bind(next_before_seq)
            .fetch_optional(pool.get())
            .await
            .map_err(internal_error)?
            .is_some(),
            (_, None) => false,
        }
    } else {
        false
    };
    Ok(Json(TranscriptResponse {
        session_id,
        items,
        page_refs,
        next_before_seq,
        has_more,
    }))
}

fn decode_transcript_item(row: &impl RowExt) -> Result<TranscriptItemResponse, String> {
    let run_id = session_row_optional_string(row, "run_id")?;
    let role = session_row_string(row, "role")?;
    let payload = session_row_optional_string(row, "payload_json")?
        .map(|json| serde_json::from_str::<TranscriptPersistPayload>(&json))
        .transpose()
        .map_err(|error| format!("decode transcript payload: {error}"))?
        .unwrap_or_default();
    Ok(TranscriptItemResponse {
        session_id: session_row_string(row, "session_id")?,
        item_seq: session_row_i64(row, "item_seq")?,
        run_id,
        role,
        content: session_row_string(row, "content")?,
        reasoning: payload.reasoning,
        reasoning_status: payload.reasoning_status,
        tool_calls: payload.tool_calls,
        tool_result: payload.tool_result,
        evidence: payload.evidence,
        source_event_id: session_row_optional_string(row, "source_event_id")?,
        created_at: session_row_string(row, "created_at")?,
    })
}

async fn load_transcript_page_refs(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    start_item_seq: Option<i64>,
    end_item_seq: Option<i64>,
) -> Result<Vec<TranscriptPageRefResponse>, String> {
    let (Some(start_item_seq), Some(end_item_seq)) = (start_item_seq, end_item_seq) else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        "SELECT tp.page_seq, tp.start_item_seq, tp.end_item_seq, tp.item_count, tp.page_hash
         FROM transcript_pages tp
         WHERE tp.user_id = ? AND tp.session_id = ? AND tp.end_item_seq >= ? AND tp.start_item_seq <= ?
         ORDER BY tp.page_seq ASC",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(start_item_seq)
    .bind(end_item_seq)
    .fetch_all(pool.get())
    .await
    .map_err(|error| error.to_string())?;
    rows.into_iter()
        .map(|row| decode_transcript_page_ref(&row))
        .collect()
}

fn decode_transcript_page_ref(row: &impl RowExt) -> Result<TranscriptPageRefResponse, String> {
    Ok(TranscriptPageRefResponse {
        page_seq: session_row_i64(row, "page_seq")?,
        start_item_seq: session_row_i64(row, "start_item_seq")?,
        end_item_seq: session_row_i64(row, "end_item_seq")?,
        item_count: session_row_i64(row, "item_count")?,
        page_hash: session_row_string(row, "page_hash")?,
    })
}

fn decode_transcript_page_stats(row: &impl RowExt) -> Result<(u32, i64), String> {
    Ok((
        session_row_u32(row, "page_count")?,
        session_row_non_negative_i64(row, "page_high_watermark")?,
    ))
}

fn decode_state_category_summary(
    row: &impl RowExt,
) -> Result<StateCategorySummaryResponse, String> {
    Ok(StateCategorySummaryResponse {
        category: session_row_string(row, "category")?,
        count: session_row_u32(row, "total")?,
    })
}

fn decode_context_manifest_summary(
    row: &impl RowExt,
) -> Result<ContextManifestSummaryResponse, String> {
    Ok(ContextManifestSummaryResponse {
        manifest_id: session_row_string(row, "manifest_id")?,
        run_id: session_row_optional_string(row, "run_id")?,
        turn_id: session_row_string(row, "turn_id")?,
        reason: session_row_string(row, "reason")?,
        total_estimated_tokens: session_row_u32(row, "total_estimated_tokens")?,
        budget_template_id: session_row_optional_string(row, "budget_template_id")?,
        policy_version: session_row_string(row, "policy_version")?,
        created_at: session_row_string(row, "created_at")?,
    })
}

fn decode_active_run_projection(row: &impl RowExt) -> Result<ActiveRunProjection, String> {
    let last_event_idx = session_row_i64(row, "last_event_idx")?;
    if last_event_idx < -1 {
        return Err(format!(
            "session row decode column `last_event_idx` expected -1 or greater, got {last_event_idx}"
        ));
    }
    Ok(ActiveRunProjection {
        run_id: session_row_string(row, "run_id")?,
        run_event_high_watermark: last_event_idx.max(0),
        replay_required: false,
        replay_start_event_idx: 0,
    })
}

async fn load_session_projection_observability(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    transcript_high_watermark: i64,
    active_run: Option<&ActiveRunProjection>,
) -> Result<SessionProjectionObservabilityResponse, (StatusCode, Json<ErrorResponse>)> {
    let transcript_page_row = sqlx::query(
        "SELECT COUNT(*) AS page_count, COALESCE(MAX(tp.end_item_seq), 0) AS page_high_watermark
         FROM transcript_pages tp
         WHERE tp.user_id = ? AND tp.session_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(pool.get())
    .await
    .map_err(internal_error)?;
    let (transcript_page_count, transcript_page_high_watermark) =
        decode_transcript_page_stats(&transcript_page_row).map_err(internal_error)?;
    let active_run_projection_lag_events = if let Some(active_run) = active_run {
        let row = sqlx::query(
            "SELECT projection_event_idx
             FROM run_display_projections
             WHERE run_id = ? AND user_id = ?",
        )
        .bind(&active_run.run_id)
        .bind(user_id)
        .fetch_optional(pool.get())
        .await
        .map_err(internal_error)?;
        let projection_event_idx = row
            .map(|row| session_row_i64(&row, "projection_event_idx"))
            .transpose()
            .map_err(internal_error)?
            .unwrap_or(-1);
        (active_run.run_event_high_watermark - projection_event_idx).max(0)
    } else {
        0
    };
    let prompt_request_count =
        astra_services::count_prompt_requests_for_session(pool, user_id, session_id)
            .await
            .map_err(internal_error)?;
    let latest_prompt_request =
        astra_services::load_latest_prompt_observability_for_session(pool, user_id, session_id)
            .await
            .map_err(internal_error)?
            .map(|request| PromptRequestObservabilityResponse {
                request_id: request.request_id,
                request_hash: request.request_hash,
                message_count: request.message_count,
                tool_count: request.tool_count,
                delta_counts: request.delta_counts,
            });
    Ok(SessionProjectionObservabilityResponse {
        observability_available: true,
        transcript_page_count,
        transcript_page_high_watermark,
        transcript_page_lag_items: (transcript_high_watermark - transcript_page_high_watermark)
            .max(0),
        active_run_projection_lag_events,
        prompt_request_count,
        latest_prompt_request,
    })
}

fn workspace_authority_from_artifact(
    artifact: &StoredSessionArtifact,
) -> Result<WorkspaceAuthorityResponse, String> {
    let metadata = serde_json::from_value::<WorkspaceMetadata>(artifact.content.clone())
        .map_err(|error| format!("workspace authority artifact decode failed: {error}"))?;
    Ok(WorkspaceAuthorityResponse {
        cwd: metadata.cwd,
        git_root: metadata.git_root,
        git_branch: metadata.git_branch,
        git_head: metadata.git_head,
        model: metadata.model,
        updated_at: metadata.updated_at,
    })
}

async fn load_workspace_authority(
    state: &AppState,
    user_id: &str,
    session_id: &str,
) -> Result<Option<WorkspaceAuthorityResponse>, (StatusCode, Json<ErrorResponse>)> {
    let artifact = session_artifact_store(state)?
        .load_latest_json_artifact(user_id, session_id, WORKSPACE_METADATA_ARTIFACT_KIND)
        .await
        .map_err(internal_error)?;
    artifact
        .as_ref()
        .map(workspace_authority_from_artifact)
        .transpose()
        .map_err(internal_error)
}

async fn load_state_summary(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
) -> Result<Vec<StateCategorySummaryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let rows = sqlx::query(
        "SELECT category, COUNT(*) AS total
         FROM session_state_items
         WHERE session_id = ? AND user_id = ? AND status IN ('active', 'backlog')
         GROUP BY category
         ORDER BY total DESC, category ASC
         LIMIT 16",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_all(pool.get())
    .await
    .map_err(internal_error)?;
    rows.into_iter()
        .map(|row| decode_state_category_summary(&row).map_err(internal_error))
        .collect()
}

async fn load_artifact_previews(
    state: &AppState,
    user_id: &str,
    session_id: &str,
) -> Result<Vec<ArtifactPreviewResponse>, (StatusCode, Json<ErrorResponse>)> {
    let artifacts = session_artifact_store(state)?
        .list_json_artifacts(user_id, session_id, None, 8, None)
        .await
        .map_err(internal_error)?
        .artifacts;
    Ok(artifacts
        .into_iter()
        .filter(|artifact| artifact.artifact_kind != WORKSPACE_METADATA_ARTIFACT_KIND)
        .take(5)
        .map(|artifact| ArtifactPreviewResponse {
            artifact_id: artifact.artifact_id,
            artifact_kind: artifact.artifact_kind,
            source: artifact.source,
            turn: artifact.turn,
            round: artifact.round,
            created_at: artifact.created_at,
        })
        .collect())
}

async fn load_latest_context_manifest(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    preferred_run_id: Option<&str>,
) -> Result<Option<ContextManifestSummaryResponse>, (StatusCode, Json<ErrorResponse>)> {
    if let Some(run_id) = preferred_run_id
        && let Some(summary) =
            fetch_latest_context_manifest(pool, user_id, session_id, Some(run_id)).await?
    {
        return Ok(Some(summary));
    }
    fetch_latest_context_manifest(pool, user_id, session_id, None).await
}

async fn fetch_latest_context_manifest(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    run_id: Option<&str>,
) -> Result<Option<ContextManifestSummaryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let row = if let Some(run_id) = run_id {
        sqlx::query(
            "SELECT manifest_id, run_id, turn_id, reason, total_estimated_tokens,
                    budget_template_id, policy_version,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at
             FROM context_manifests
             WHERE user_id = ? AND session_id = ? AND run_id = ?
             ORDER BY created_at DESC, manifest_id DESC
             LIMIT 1",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .fetch_optional(pool.get())
        .await
        .map_err(internal_error)?
    } else {
        sqlx::query(
            "SELECT manifest_id, run_id, turn_id, reason, total_estimated_tokens,
                    budget_template_id, policy_version,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at
             FROM context_manifests
             WHERE user_id = ? AND session_id = ?
             ORDER BY created_at DESC, manifest_id DESC
             LIMIT 1",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_optional(pool.get())
        .await
        .map_err(internal_error)?
    };
    row.map(|row| decode_context_manifest_summary(&row).map_err(internal_error))
        .transpose()
}

pub(crate) async fn update_session_handler(
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
                metadata_patch: request.metadata_patch,
                status: request.status,
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(crate) async fn list_session_devices_handler(
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

    let devices = rows
        .into_iter()
        .map(|row| decode_device_response(&row))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            internal_error(format!(
                "decode session device leases failed for session {session_id}: {error}"
            ))
        })?;

    Ok(Json(DeviceListResponse {
        session_id,
        devices,
    }))
}

pub(crate) async fn enroll_session_device_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<DeviceEnrollRequest>,
) -> Result<(StatusCode, Json<DeviceEnrollResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;
    let device_id = validate_device_protocol_value("device_id", &request.device_id)?;
    let device_fingerprint =
        validate_device_protocol_value("device_fingerprint", &request.device_fingerprint)?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;

    let existing = sqlx::query(
        "SELECT device_key_hash
         FROM session_device_leases
         WHERE user_id = ? AND session_id = ? AND device_id = ?
         LIMIT 1",
    )
    .bind(&user.user_id)
    .bind(&session_id)
    .bind(device_id)
    .fetch_optional(pool.get())
    .await
    .map_err(internal_error)?;

    let device_key = new_device_secret("dk");
    let device_key_hash = sha256_raw_hex(device_key.as_bytes());
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(DEVICE_LEASE_TTL_HOURS);
    let status = if let Some(row) = existing {
        let prior_key_hash = session_row_string(&row, "device_key_hash").map_err(internal_error)?;
        let proof = request.reauthentication_proof.as_deref().ok_or_else(|| {
            error_response(
                StatusCode::FORBIDDEN,
                "verified reauthentication is required to re-enroll a device",
            )
        })?;
        state
            .auth_service
            .consume_reauthentication_proof(
                &user.user_id,
                astra_services::auth::ReauthenticationPurpose::DeviceReenroll,
                proof,
            )
            .await?;
        let result = sqlx::query(
            "UPDATE session_device_leases
             SET device_fingerprint = ?, device_key_hash = ?, trust_level = 'new_device',
                 status = 'active', last_monotonic_id = 0, expires_at = ?,
                 revoked_at = NULL, updated_at = NOW(6)
             WHERE user_id = ? AND session_id = ? AND device_id = ? AND device_key_hash = ?",
        )
        .bind(device_fingerprint)
        .bind(&device_key_hash)
        .bind(expires_at.naive_utc())
        .bind(&user.user_id)
        .bind(&session_id)
        .bind(device_id)
        .bind(prior_key_hash)
        .execute(pool.get())
        .await
        .map_err(internal_error)?;
        if result.rows_affected() != 1 {
            return Err(error_response(
                StatusCode::CONFLICT,
                "device changed during verified re-enrollment",
            ));
        }
        StatusCode::OK
    } else {
        let result = sqlx::query(
            "INSERT INTO session_device_leases
             (lease_id, user_id, session_id, device_id, device_fingerprint, device_key_hash,
              trust_level, status, last_monotonic_id, expires_at, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 'new_device', 'active', 0, ?, NOW(6), NOW(6))",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&user.user_id)
        .bind(&session_id)
        .bind(device_id)
        .bind(device_fingerprint)
        .bind(&device_key_hash)
        .bind(expires_at.naive_utc())
        .execute(pool.get())
        .await;
        match result {
            Ok(_) => StatusCode::CREATED,
            Err(error) if is_duplicate_key_error(&error) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "device enrollment raced with another request; retry with verified reauthentication",
                ));
            }
            Err(error) => return Err(internal_error(error)),
        }
    };

    sqlx::query(
        "DELETE FROM session_device_challenges
         WHERE user_id = ? AND session_id = ? AND device_id = ?",
    )
    .bind(&user.user_id)
    .bind(&session_id)
    .bind(device_id)
    .execute(pool.get())
    .await
    .map_err(internal_error)?;

    let lease = load_device_response(pool, &user.user_id, &session_id, device_id).await?;
    Ok((status, Json(DeviceEnrollResponse { lease, device_key })))
}

pub(crate) async fn create_session_device_challenge_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<DeviceChallengeRequest>,
) -> Result<Json<DeviceChallengeResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;
    let device_id = validate_device_protocol_value("device_id", &request.device_id)?;
    let device_fingerprint =
        validate_device_protocol_value("device_fingerprint", &request.device_fingerprint)?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
    let active = sqlx::query(
        "SELECT 1
         FROM session_device_leases
         WHERE user_id = ? AND session_id = ? AND device_id = ? AND device_fingerprint = ?
           AND status = 'active' AND expires_at > NOW(6)
         LIMIT 1",
    )
    .bind(&user.user_id)
    .bind(&session_id)
    .bind(device_id)
    .bind(device_fingerprint)
    .fetch_optional(pool.get())
    .await
    .map_err(internal_error)?
    .is_some();
    if !active {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "active device enrollment is required",
        ));
    }

    let challenge_id = Uuid::new_v4().to_string();
    let challenge = new_device_secret("dc");
    let challenge_digest = sha256_raw_hex(challenge.as_bytes());
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEVICE_CHALLENGE_TTL_SECONDS);
    sqlx::query(
        "INSERT INTO session_device_challenges
         (challenge_id, user_id, session_id, device_id, device_fingerprint, purpose,
          challenge_digest, expires_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(&challenge_id)
    .bind(&user.user_id)
    .bind(&session_id)
    .bind(device_id)
    .bind(device_fingerprint)
    .bind(request.purpose.as_str())
    .bind(challenge_digest)
    .bind(expires_at.naive_utc())
    .execute(pool.get())
    .await
    .map_err(internal_error)?;

    Ok(Json(DeviceChallengeResponse {
        challenge_id,
        challenge,
        purpose: request.purpose,
        expires_in: DEVICE_CHALLENGE_TTL_SECONDS as u32,
    }))
}

pub(crate) async fn revoke_session_device_handler(
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
    update.push(" AND user_id = ");
    update.push_bind(&lease.user_id);
    update.push(" AND session_id = ");
    update.push_bind(&lease.session_id);
    update.push(" AND device_id = ");
    update.push_bind(&lease.device_id);
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
    sqlx::query(
        "DELETE FROM session_device_challenges
         WHERE user_id = ? AND session_id = ? AND device_id = ?",
    )
    .bind(&user.user_id)
    .bind(&session_id)
    .bind(&lease.device_id)
    .execute(pool.get())
    .await
    .map_err(internal_error)?;

    let payload = insert_device_lease_event(
        pool,
        &lease,
        "device_revoked",
        request.reason.as_deref().unwrap_or("explicit_revoke"),
    )
    .await?;
    if let Ok(value) = serde_json::to_value(&payload) {
        super::device_lease_sweeper::publish_device_lease_event(&user.user_id, &session_id, value);
    }
    Ok(Json(DeviceRevokeResponse {
        event: payload,
        idempotent: false,
    }))
}

pub(crate) async fn trust_session_device_handler(
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
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
    let device_id = validate_device_protocol_value("device_id", &request.device_id)?;
    let device_fingerprint =
        validate_device_protocol_value("device_fingerprint", &request.device_fingerprint)?;
    let challenge_id = validate_device_protocol_value("challenge_id", &request.challenge_id)?;
    validate_device_proof_value(&request.device_proof)?;
    let verified = verify_device_proof(
        pool,
        &user.user_id,
        &session_id,
        device_id,
        device_fingerprint,
        challenge_id,
        &request.device_proof,
        DeviceProofPurpose::Trust,
    )
    .await?;
    debug_assert_eq!(verified.device_id, device_id);
    state
        .auth_service
        .consume_reauthentication_proof(
            &user.user_id,
            astra_services::auth::ReauthenticationPurpose::DeviceTrust,
            &request.reauthentication_proof,
        )
        .await?;

    let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "UPDATE session_device_leases
         SET trust_level = 'trusted', updated_at = NOW(6)
         WHERE session_id = ",
    );
    update.push_bind(&session_id);
    update.push(" AND user_id = ");
    update.push_bind(&user.user_id);
    update.push(" AND device_id = ");
    update.push_bind(device_id);
    update.push(" AND device_fingerprint = ");
    update.push_bind(device_fingerprint);
    update.push(" AND status = 'active' AND expires_at > NOW(6) AND trust_level = 'new_device'");
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
    .bind(device_id)
    .fetch_one(pool.get())
    .await
    .map_err(internal_error)?;
    let lease = decode_device_response(&row).map_err(|error| {
        internal_error(format!(
            "decode trusted device lease failed for session {session_id}: {error}"
        ))
    })?;
    Ok(Json(DeviceTrustResponse { lease }))
}

pub(crate) async fn session_device_events_handler(
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
            if !event.belongs_to(&user.user_id, &session_id) {
                continue;
            }
            yield Ok::<_, std::convert::Infallible>(format!(
                "data: {}\n\n",
                serde_json::to_string(&event.payload).unwrap_or_default()
            ));
        }
    };
    bridge::sse_stream_response(StatusCode::OK, Body::from_stream(stream))
}

pub(crate) async fn delete_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id;
    state
        .session_service
        .delete_session(session_id.clone(), user_id.clone())
        .await?;
    if let Err(error) =
        astra_services::session_journal::delete_session_for_user(&user_id, &session_id)
    {
        tracing::warn!(
            user_id,
            session_id,
            %error,
            "database session deleted but owner-scoped local diagnostics cleanup failed"
        );
    }
    astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn close_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id.clone(),
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                metadata_patch: None,
                status: Some("closed".to_string()),
            },
        )
        .await?;
    astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
    Ok(Json(SessionResponse::from(session)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionAttachRequest {
    pub idempotency_key: String,
    pub placement: astra_turn_types::SessionPlacementV1,
    #[serde(default)]
    pub after_manifest_root: Option<String>,
    #[serde(default = "default_attachment_ttl_seconds")]
    pub ttl_seconds: u64,
}

fn default_attachment_ttl_seconds() -> u64 {
    15 * 60
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionHandoffRequest {
    pub idempotency_key: String,
    pub mode: astra_turn_types::SessionHandoffModeV1,
    #[serde(default)]
    pub from_attachment_id: Option<String>,
    pub to_attachment_id: String,
    #[serde(default)]
    pub from_placement: Option<astra_turn_types::SessionPlacementV1>,
    #[serde(default)]
    pub expected_cursor: Option<astra_turn_types::SessionCursorV1>,
    #[serde(default)]
    pub watermarks: astra_turn_types::HandoffOperationWatermarksV1,
    #[serde(default)]
    pub unsynced_suffix_root: Option<String>,
    #[serde(default)]
    pub unknown_effect_invocation_ids: Vec<String>,
    #[serde(default)]
    pub reauthentication_proof: Option<String>,
    pub reason: String,
    #[serde(default = "default_handoff_deadline_seconds")]
    pub deadline_seconds: u64,
}

fn default_handoff_deadline_seconds() -> u64 {
    5 * 60
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionHandoffTransitionRequest {
    pub idempotency_key: String,
    pub expected_state: astra_turn_types::SessionHandoffStateV1,
    pub expected_transition_seq: u64,
    pub next_state: astra_turn_types::SessionHandoffStateV1,
    #[serde(default)]
    pub patch: astra_services::HandoffTransitionPatchV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionHandoffFenceRequest {
    pub idempotency_key: String,
    #[serde(default)]
    pub source_lease: Option<astra_turn_types::ConversationWriterLeaseV1>,
    #[serde(default = "default_writer_ttl_seconds")]
    pub writer_ttl_seconds: u64,
}

fn default_writer_ttl_seconds() -> u64 {
    5 * 60
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionHandoffActivateRequest {
    pub idempotency_key: String,
    pub expected_transition_seq: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSegmentBatchRequest {
    pub segment_hashes: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct SessionSegmentBatchResponse {
    pub segments: Vec<astra_turn_types::ConversationSegmentV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSegmentUploadRequest {
    pub segments: Vec<astra_turn_types::ConversationSegmentV1>,
}

#[derive(Serialize)]
pub(crate) struct SessionSegmentUploadResponse {
    pub stored_segment_hashes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionPublishRequest {
    pub idempotency_key: String,
    pub items: Vec<astra_services::PublishJournalItemV1>,
    #[serde(default = "default_writer_ttl_seconds")]
    pub writer_ttl_seconds: u64,
}

pub(crate) async fn resume_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<astra_services::AttachSessionOutcomeV1>, (StatusCode, Json<ErrorResponse>)> {
    attach_session(
        &state,
        &session_id,
        &headers,
        SessionAttachRequest {
            idempotency_key: format!("resume-server:{}", state.session_actor_id),
            placement: astra_turn_types::SessionPlacementV1::Server,
            after_manifest_root: None,
            ttl_seconds: default_attachment_ttl_seconds(),
        },
    )
    .await
    .map(Json)
}

pub(crate) async fn attach_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SessionAttachRequest>,
) -> Result<Json<astra_services::AttachSessionOutcomeV1>, (StatusCode, Json<ErrorResponse>)> {
    attach_session(&state, &session_id, &headers, request)
        .await
        .map(Json)
}

async fn attach_session(
    state: &AppState,
    session_id: &str,
    headers: &HeaderMap,
    request: SessionAttachRequest,
) -> Result<astra_services::AttachSessionOutcomeV1, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(headers).await?;
    let session = state
        .session_service
        .get_session(session_id.to_owned(), user.user_id.clone())
        .await?;
    let key = server_session_key(&user.user_id, &session.session_id);
    let coordinator = state
        .session_context_coordinator
        .as_ref()
        .ok_or_else(|| internal_error("session context coordinator is not configured"))?;
    let epochs = coordinator
        .load_authority_epochs(&key)
        .await
        .map_err(session_context_http_error)?
        .unwrap_or_default();
    let (actor_kind, surface, actor_id, device_id) = match request.placement {
        astra_turn_types::SessionPlacementV1::Server => (
            astra_turn_types::ActorKindV1::Server,
            astra_turn_types::SessionSurfaceV1::Server,
            state.session_actor_id.clone(),
            None,
        ),
        astra_turn_types::SessionPlacementV1::Cli | astra_turn_types::SessionPlacementV1::Edge => {
            let pool = state
                .shared_pool
                .as_ref()
                .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
            let device = verify_device_proof_from_headers(
                pool,
                &user.user_id,
                &session.session_id,
                headers,
                DeviceProofPurpose::Hydrate,
            )
            .await?;
            let (kind, surface) = if request.placement == astra_turn_types::SessionPlacementV1::Cli
            {
                (
                    astra_turn_types::ActorKindV1::Cli,
                    astra_turn_types::SessionSurfaceV1::Cli,
                )
            } else {
                (
                    astra_turn_types::ActorKindV1::Edge,
                    astra_turn_types::SessionSurfaceV1::Edge,
                )
            };
            (
                kind,
                surface,
                format!("device:{}", device.device_id),
                Some(device.device_id),
            )
        }
    };
    let actor = astra_turn_types::ActorContextV1::owner_user(
        &user.user_id,
        actor_id,
        actor_kind,
        surface,
        device_id,
        epochs,
    );
    let service = handoff_service(state)?;
    service
        .attach_read_only(
            &astra_services::AttachSessionRequestV1 {
                idempotency_key: request.idempotency_key,
                key,
                actor,
                placement: request.placement,
                after_manifest_root: request.after_manifest_root,
                // Workspace authority is never accepted as an unsigned HTTP
                // body claim. A later capability-bound attach supplies it
                // through the trusted internal service boundary.
                workspace: None,
            },
            std::time::Duration::from_secs(request.ttl_seconds),
        )
        .await
        .map_err(session_handoff_http_error)
}

pub(crate) async fn request_session_handoff_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SessionHandoffRequest>,
) -> Result<Json<astra_turn_types::SessionHandoffRecordV1>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    let key = server_session_key(&user.user_id, &session.session_id);
    let service = handoff_service(&state)?;
    let target = service
        .load_attachment(&key, &request.to_attachment_id)
        .await
        .map_err(session_handoff_http_error)?;
    let source = match request.from_attachment_id.as_deref() {
        Some(id) => Some(
            service
                .load_attachment(&key, id)
                .await
                .map_err(session_handoff_http_error)?,
        ),
        None => None,
    };
    let from_placement = source
        .as_ref()
        .map(|attachment| attachment.placement)
        .or(request.from_placement)
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "from_placement is required when no source attachment is available",
            )
        })?;
    let coordinator = state
        .session_context_coordinator
        .as_ref()
        .ok_or_else(|| internal_error("session context coordinator is not configured"))?;
    let head = coordinator
        .load_head(&key)
        .await
        .map_err(session_context_http_error)?;
    if request.expected_cursor.as_ref() != head.as_ref().map(|head| &head.cursor) {
        return Err(error_response(
            StatusCode::CONFLICT,
            "expected cursor is not the current Server head",
        ));
    }
    let authority_epochs = coordinator
        .load_authority_epochs(&key)
        .await
        .map_err(session_context_http_error)?
        .unwrap_or_default();
    if target.actor.authority_epochs != authority_epochs {
        return Err(error_response(
            StatusCode::CONFLICT,
            "target attachment authority is stale; attach again",
        ));
    }
    let risk = match request.mode {
        astra_turn_types::SessionHandoffModeV1::Graceful => {
            astra_turn_types::HandoffRiskEvidenceV1::default()
        }
        astra_turn_types::SessionHandoffModeV1::Forced => {
            let proof = request.reauthentication_proof.as_deref().ok_or_else(|| {
                error_response(
                    StatusCode::FORBIDDEN,
                    "forced takeover requires verified reauthentication",
                )
            })?;
            state
                .auth_service
                .consume_reauthentication_proof(
                    &user.user_id,
                    astra_services::ReauthenticationPurpose::SessionForcedTakeover,
                    proof,
                )
                .await?;
            astra_turn_types::HandoffRiskEvidenceV1 {
                unsynced_suffix_root: request.unsynced_suffix_root,
                unknown_effect_invocation_ids: request.unknown_effect_invocation_ids,
                forced_authorization_id: Some(format!(
                    "consumed-reauth:{}",
                    sha256_hex(proof.as_bytes())
                )),
            }
        }
    };
    service
        .request_handoff(
            &astra_services::RequestSessionHandoffV1 {
                idempotency_key: request.idempotency_key,
                key,
                mode: request.mode,
                from_attachment_id: request.from_attachment_id,
                to_attachment_id: request.to_attachment_id,
                from_placement,
                to_placement: target.placement,
                target_actor: target.actor,
                base_cursor: head.map(|head| head.cursor),
                authority_epochs,
                workspace: target.workspace,
                watermarks: request.watermarks,
                risk,
                reason: request.reason,
            },
            std::time::Duration::from_secs(request.deadline_seconds),
        )
        .await
        .map(Json)
        .map_err(session_handoff_http_error)
}

pub(crate) async fn get_session_handoff_handler(
    State(state): State<AppState>,
    Path((session_id, handoff_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<astra_turn_types::SessionHandoffRecordV1>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    handoff_service(&state)?
        .load_handoff(
            &server_session_key(&user.user_id, &session.session_id),
            &handoff_id,
        )
        .await
        .map(Json)
        .map_err(session_handoff_http_error)
}

pub(crate) async fn transition_session_handoff_handler(
    State(state): State<AppState>,
    Path((session_id, handoff_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<SessionHandoffTransitionRequest>,
) -> Result<Json<astra_turn_types::SessionHandoffRecordV1>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    handoff_service(&state)?
        .transition_handoff(&astra_services::TransitionSessionHandoffV1 {
            idempotency_key: request.idempotency_key,
            key: server_session_key(&user.user_id, &session.session_id),
            handoff_id,
            expected_state: request.expected_state,
            expected_transition_seq: request.expected_transition_seq,
            next_state: request.next_state,
            patch: request.patch,
        })
        .await
        .map(Json)
        .map_err(session_handoff_http_error)
}

pub(crate) async fn fence_session_handoff_handler(
    State(state): State<AppState>,
    Path((session_id, handoff_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<SessionHandoffFenceRequest>,
) -> Result<Json<astra_services::FenceSessionWriterOutcomeV1>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    handoff_service(&state)?
        .fence_writer(
            &server_session_key(&user.user_id, &session.session_id),
            &handoff_id,
            request.source_lease,
            std::time::Duration::from_secs(request.writer_ttl_seconds),
            &request.idempotency_key,
        )
        .await
        .map(Json)
        .map_err(session_handoff_http_error)
}

pub(crate) async fn activate_session_handoff_handler(
    State(state): State<AppState>,
    Path((session_id, handoff_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<SessionHandoffActivateRequest>,
) -> Result<Json<astra_turn_types::SessionHandoffRecordV1>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    handoff_service(&state)?
        .activate_handoff(
            &server_session_key(&user.user_id, &session.session_id),
            &handoff_id,
            request.expected_transition_seq,
            &request.idempotency_key,
        )
        .await
        .map(Json)
        .map_err(session_handoff_http_error)
}

pub(crate) async fn get_session_segments_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SessionSegmentBatchRequest>,
) -> Result<Json<SessionSegmentBatchResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    let coordinator = state
        .session_context_coordinator
        .as_ref()
        .ok_or_else(|| internal_error("session context coordinator is not configured"))?;
    let segments = coordinator
        .load_segments(
            &server_session_key(&user.user_id, &session.session_id),
            &request.segment_hashes,
        )
        .await
        .map_err(session_context_http_error)?;
    Ok(Json(SessionSegmentBatchResponse { segments }))
}

pub(crate) async fn upload_session_segments_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SessionSegmentUploadRequest>,
) -> Result<Json<SessionSegmentUploadResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
    let _device = verify_device_proof_from_headers(
        pool,
        &user.user_id,
        &session.session_id,
        &headers,
        DeviceProofPurpose::Publish,
    )
    .await?;
    let key = server_session_key(&user.user_id, &session.session_id);
    publish_service(&state)?
        .store_segments(&key, &request.segments)
        .await
        .map_err(session_publish_http_error)?;
    Ok(Json(SessionSegmentUploadResponse {
        stored_segment_hashes: request
            .segments
            .into_iter()
            .map(|segment| segment.segment_hash)
            .collect(),
    }))
}

pub(crate) async fn publish_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SessionPublishRequest>,
) -> Result<Json<astra_services::PublishSessionOutcomeV1>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;
    let device = verify_device_proof_from_headers(
        pool,
        &user.user_id,
        &session.session_id,
        &headers,
        DeviceProofPurpose::Publish,
    )
    .await?;
    let key = server_session_key(&user.user_id, &session.session_id);
    let coordinator = state
        .session_context_coordinator
        .as_ref()
        .ok_or_else(|| internal_error("session context coordinator is not configured"))?;
    let epochs = coordinator
        .load_authority_epochs(&key)
        .await
        .map_err(session_context_http_error)?
        .unwrap_or_default();
    let actor = astra_turn_types::ActorContextV1::owner_user(
        &user.user_id,
        format!("device:{}", device.device_id),
        astra_turn_types::ActorKindV1::Cli,
        astra_turn_types::SessionSurfaceV1::Cli,
        Some(device.device_id),
        epochs,
    );
    publish_service(&state)?
        .publish(
            &astra_services::PublishSessionRequestV1 {
                idempotency_key: request.idempotency_key,
                key,
                actor,
                items: request.items,
            },
            std::time::Duration::from_secs(request.writer_ttl_seconds),
        )
        .await
        .map(Json)
        .map_err(session_publish_http_error)
}

fn server_session_key(user_id: &str, session_id: &str) -> astra_turn_types::SessionKeyV1 {
    astra_turn_types::SessionKeyV1::owner_session(
        "server",
        user_id,
        session_id,
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    )
}

fn publish_service(
    state: &AppState,
) -> Result<&astra_services::DatabaseSessionPublishService, (StatusCode, Json<ErrorResponse>)> {
    state
        .session_publish_service
        .as_deref()
        .ok_or_else(|| internal_error("session publish service is not configured"))
}

fn handoff_service(
    state: &AppState,
) -> Result<&astra_services::DatabaseSessionHandoffService, (StatusCode, Json<ErrorResponse>)> {
    state
        .session_handoff_service
        .as_deref()
        .ok_or_else(|| internal_error("session handoff service is not configured"))
}

fn session_context_http_error(
    error: astra_services::SessionContextCoordinatorError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        astra_services::SessionContextCoordinatorError::DivergentManifest => error_response(
            StatusCode::CONFLICT,
            "observed manifest is not an ancestor of the current Server head; fork is required",
        ),
        astra_services::SessionContextCoordinatorError::Invalid(message) => {
            error_response(StatusCode::BAD_REQUEST, message)
        }
        astra_services::SessionContextCoordinatorError::SegmentNotFound => {
            error_response(StatusCode::NOT_FOUND, error.to_string())
        }
        astra_services::SessionContextCoordinatorError::Fenced
        | astra_services::SessionContextCoordinatorError::Expired => {
            error_response(StatusCode::CONFLICT, error.to_string())
        }
        _ => internal_error(error),
    }
}

fn session_handoff_http_error(
    error: astra_services::SessionHandoffError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        astra_services::SessionHandoffError::Invalid(message) => {
            error_response(StatusCode::BAD_REQUEST, message)
        }
        astra_services::SessionHandoffError::NotFound => {
            error_response(StatusCode::NOT_FOUND, error.to_string())
        }
        astra_services::SessionHandoffError::ForkRequired { .. } => {
            error_response_coded(StatusCode::CONFLICT, error.to_string(), "fork_required")
        }
        astra_services::SessionHandoffError::ActiveHandoffConflict { .. }
        | astra_services::SessionHandoffError::StateConflict { .. }
        | astra_services::SessionHandoffError::IdempotencyMismatch
        | astra_services::SessionHandoffError::DeadlineExpired
        | astra_services::SessionHandoffError::AttachmentExpired => {
            error_response(StatusCode::CONFLICT, error.to_string())
        }
        astra_services::SessionHandoffError::Coordinator(source) => {
            session_context_http_error(source)
        }
        _ => internal_error(error),
    }
}

fn session_publish_http_error(
    error: astra_services::SessionPublishError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        astra_services::SessionPublishError::Invalid(message) => {
            error_response_coded(StatusCode::BAD_REQUEST, message, "session_publish_invalid")
        }
        astra_services::SessionPublishError::MissingSegment => error_response_coded(
            StatusCode::CONFLICT,
            error.to_string(),
            "session_publish_incomplete",
        ),
        astra_services::SessionPublishError::UnacknowledgedJournal => error_response_coded(
            StatusCode::CONFLICT,
            error.to_string(),
            "session_publish_journal_unacknowledged",
        ),
        astra_services::SessionPublishError::ForkRequired { .. } => {
            error_response_coded(StatusCode::CONFLICT, error.to_string(), "fork_required")
        }
        astra_services::SessionPublishError::Conflict => error_response_coded(
            StatusCode::CONFLICT,
            error.to_string(),
            "session_publish_conflict",
        ),
        astra_services::SessionPublishError::Coordinator(
            astra_services::SessionContextCoordinatorError::Invalid(message),
        ) => error_response_coded(StatusCode::BAD_REQUEST, message, "session_publish_invalid"),
        astra_services::SessionPublishError::Coordinator(
            astra_services::SessionContextCoordinatorError::Fenced
            | astra_services::SessionContextCoordinatorError::Expired,
        ) => error_response_coded(
            StatusCode::CONFLICT,
            error.to_string(),
            "session_publish_fenced",
        ),
        _ => internal_error(error),
    }
}

pub(crate) async fn cancel_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id.clone();
    let session_id_for_cancel = session_id.clone();
    let _ = state
        .session_service
        .get_session(session_id.clone(), user_id.clone())
        .await?;
    state
        .execution
        .run_lifecycle_service
        .cancel_session_runs(session_id_for_cancel, user_id.clone())
        .await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                metadata_patch: None,
                status: Some(STATUS_CANCELLED.to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(crate) async fn session_activity_handler(
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
        .get_session_activity(session_id, user.user_id, query.limit, query.cursor()?)
        .await?;
    Ok(Json(SessionActivityResponse::from(activities)))
}

pub(crate) async fn list_session_artifacts_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionArtifactListQuery>,
) -> Result<Json<SessionArtifactListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id.clone();
    let _ = state
        .session_service
        .get_session(session_id.clone(), user_id.clone())
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let page = artifact_store
        .list_json_artifacts(
            &user_id,
            &session_id,
            query.artifact_kind.as_deref(),
            query.limit as usize,
            query.cursor()?,
        )
        .await
        .map_err(internal_artifact_error)?;
    let limit = page.limit as u32;
    let next_cursor = page.next_cursor;
    Ok(Json(SessionArtifactListResponse {
        session_id,
        artifacts: page
            .artifacts
            .into_iter()
            .map(session_artifact_response)
            .collect(),
        limit,
        next_cursor,
    }))
}

pub(crate) async fn get_latest_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_kind)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<SessionArtifactResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id.clone();
    let _ = state
        .session_service
        .get_session(session_id.clone(), user_id.clone())
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifact = artifact_store
        .load_latest_json_artifact(&user_id, &session_id, &artifact_kind)
        .await
        .map_err(internal_artifact_error)?
        .ok_or_else(session_artifact_not_found)?;
    Ok(Json(session_artifact_response(artifact)))
}

pub(crate) async fn get_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<SessionArtifactResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id.clone();
    let _ = state
        .session_service
        .get_session(session_id.clone(), user_id.clone())
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifact = artifact_store
        .load_json_artifact(&user_id, &session_id, &artifact_id)
        .await
        .map_err(internal_artifact_error)?
        .ok_or_else(session_artifact_not_found)?;
    Ok(Json(session_artifact_response(artifact)))
}

pub(crate) async fn download_session_artifact_handler(
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
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?
         LIMIT 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_optional(pool.get())
    .await
    .map_err(internal_artifact_error)?
    .ok_or_else(session_artifact_not_found)?;
    let status = session_row_string(&row, "status").map_err(internal_artifact_error)?;
    if !session_artifact_raw_payload_is_available(&status) {
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
        referenced_by_durable_count: artifact.referenced_by_durable_count,
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
            match update_session_state_revision(pool, &revision).await? {
                true => Ok(()),
                false => Err(sqlx::Error::RowNotFound),
            }
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
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(revision.user_id)
    .bind(revision.monotonic_id)
    .bind(revision.revision_hash)
    .bind(revision.device_fingerprint)
    .bind(revision.transcript_high_watermark)
    .bind(revision.run_event_high_watermark)
    .bind(revision.state_projection_hash)
    .bind(revision.session_id)
    .bind(revision.user_id)
    .execute(pool.get())
    .await?;
    Ok(result.rows_affected() > 0)
}

fn validate_device_protocol_value<'a>(
    field: &str,
    value: &'a str,
) -> Result<&'a str, (StatusCode, Json<ErrorResponse>)> {
    let valid = !value.is_empty()
        && value.trim() == value
        && value.len() <= DEVICE_PROTOCOL_VALUE_MAX_LEN
        && value.bytes().all(|byte| byte.is_ascii_graphic());
    if !valid {
        return Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{field} must be 1..={DEVICE_PROTOCOL_VALUE_MAX_LEN} visible ASCII bytes without padding"
            ),
        ));
    }
    Ok(value)
}

fn validate_device_proof_value(proof: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let decoded_len = URL_SAFE_NO_PAD
        .decode(proof)
        .ok()
        .map(|decoded| decoded.len());
    if proof.is_empty()
        || proof.trim() != proof
        || proof.len() > DEVICE_PROOF_MAX_LEN
        || decoded_len != Some(32)
    {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "device proof is malformed",
        ));
    }
    Ok(())
}

fn required_device_header<'a>(
    headers: &'a HeaderMap,
    name: &'static str,
) -> Result<&'a str, (StatusCode, Json<ErrorResponse>)> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            error_response(
                StatusCode::UNAUTHORIZED,
                format!("required device proof header is missing or invalid: {name}"),
            )
        })
}

fn new_device_secret(prefix: &str) -> String {
    format!(
        "{prefix}_{}_{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

fn sha256_raw_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[allow(clippy::too_many_arguments)]
fn verify_device_proof_signature(
    device_key_hash: &str,
    purpose: DeviceProofPurpose,
    user_id: &str,
    session_id: &str,
    device_id: &str,
    device_fingerprint: &str,
    challenge_id: &str,
    challenge_digest: &str,
    proof: &str,
) -> bool {
    let Ok(proof_bytes) = URL_SAFE_NO_PAD.decode(proof) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(device_key_hash.as_bytes()) else {
        return false;
    };
    mac.update(&canonical_device_proof_message(
        purpose,
        user_id,
        session_id,
        device_id,
        device_fingerprint,
        challenge_id,
        challenge_digest,
    ));
    mac.verify_slice(&proof_bytes).is_ok()
}

async fn verify_device_proof_from_headers(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    headers: &HeaderMap,
    purpose: DeviceProofPurpose,
) -> Result<VerifiedDeviceIdentity, (StatusCode, Json<ErrorResponse>)> {
    let device_id = validate_device_protocol_value(
        "device_id",
        required_device_header(headers, DEVICE_ID_HEADER)?,
    )?;
    let device_fingerprint = validate_device_protocol_value(
        "device_fingerprint",
        required_device_header(headers, DEVICE_FINGERPRINT_HEADER)?,
    )?;
    let challenge_id = validate_device_protocol_value(
        "challenge_id",
        required_device_header(headers, DEVICE_CHALLENGE_ID_HEADER)?,
    )?;
    let proof = required_device_header(headers, DEVICE_PROOF_HEADER)?;
    validate_device_proof_value(proof)?;
    verify_device_proof(
        pool,
        user_id,
        session_id,
        device_id,
        device_fingerprint,
        challenge_id,
        proof,
        purpose,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn verify_device_proof(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    device_id: &str,
    device_fingerprint: &str,
    challenge_id: &str,
    proof: &str,
    purpose: DeviceProofPurpose,
) -> Result<VerifiedDeviceIdentity, (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT c.challenge_digest, l.device_key_hash
         FROM session_device_challenges c
         JOIN session_device_leases l
           ON l.user_id = c.user_id AND l.session_id = c.session_id
          AND l.device_id = c.device_id AND l.device_fingerprint = c.device_fingerprint
         WHERE c.user_id = ? AND c.session_id = ? AND c.device_id = ?
           AND c.device_fingerprint = ? AND c.challenge_id = ? AND c.purpose = ?
           AND c.consumed_at IS NULL AND c.expires_at > NOW(6)
           AND l.status = 'active' AND l.expires_at > NOW(6)
         LIMIT 1",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(device_id)
    .bind(device_fingerprint)
    .bind(challenge_id)
    .bind(purpose.as_str())
    .fetch_optional(pool.get())
    .await
    .map_err(internal_error)?
    .ok_or_else(|| {
        error_response(
            StatusCode::FORBIDDEN,
            "device challenge is invalid, expired, consumed, or no longer active",
        )
    })?;
    let challenge_digest = session_row_string(&row, "challenge_digest").map_err(internal_error)?;
    let device_key_hash = session_row_string(&row, "device_key_hash").map_err(internal_error)?;
    if !verify_device_proof_signature(
        &device_key_hash,
        purpose,
        user_id,
        session_id,
        device_id,
        device_fingerprint,
        challenge_id,
        &challenge_digest,
        proof,
    ) {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "device proof verification failed",
        ));
    }

    let consumed = sqlx::query(
        "UPDATE session_device_challenges
         SET consumed_at = NOW(6)
         WHERE user_id = ? AND session_id = ? AND device_id = ?
           AND challenge_id = ? AND purpose = ?
           AND consumed_at IS NULL AND expires_at > NOW(6)",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(device_id)
    .bind(challenge_id)
    .bind(purpose.as_str())
    .execute(pool.get())
    .await
    .map_err(internal_error)?;
    if consumed.rows_affected() != 1 {
        return Err(error_response(
            StatusCode::CONFLICT,
            "device challenge was already consumed",
        ));
    }
    if purpose == DeviceProofPurpose::Hydrate {
        let renewed_until = chrono::Utc::now() + chrono::Duration::hours(DEVICE_LEASE_TTL_HOURS);
        let renewed = sqlx::query(
            "UPDATE session_device_leases
             SET expires_at = ?, updated_at = NOW(6)
             WHERE user_id = ? AND session_id = ? AND device_id = ?
               AND device_fingerprint = ? AND device_key_hash = ?
               AND status = 'active' AND expires_at > NOW(6)",
        )
        .bind(renewed_until.naive_utc())
        .bind(user_id)
        .bind(session_id)
        .bind(device_id)
        .bind(device_fingerprint)
        .bind(&device_key_hash)
        .execute(pool.get())
        .await
        .map_err(internal_error)?;
        if renewed.rows_affected() != 1 {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "device enrollment changed before hydration",
            ));
        }
    }

    Ok(VerifiedDeviceIdentity {
        device_id: device_id.to_string(),
        device_fingerprint: device_fingerprint.to_string(),
    })
}

async fn transcript_high_watermark(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
) -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT COALESCE(MAX(item_seq), 0) AS high_watermark
         FROM session_transcript_items
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_one(pool.get())
    .await
    .map_err(internal_error)?;
    session_row_non_negative_i64(&row, "high_watermark").map_err(internal_error)
}

async fn active_run_projection(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
) -> Result<Option<ActiveRunProjection>, (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT run_id, last_event_idx
         FROM agent_runs
         WHERE session_id = ? AND user_id = ? AND status IN ('running', 'waiting', 'paused')
         ORDER BY updated_at DESC
         LIMIT 1",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(pool.get())
    .await
    .map_err(internal_error)?;
    row.map(|row| decode_active_run_projection(&row).map_err(internal_error))
        .transpose()
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

fn decode_device_response(row: &impl RowExt) -> Result<DeviceLeaseResponse, String> {
    Ok(DeviceLeaseResponse {
        lease_id: session_row_string(row, "lease_id")?,
        session_id: session_row_string(row, "session_id")?,
        device_id: session_row_string(row, "device_id")?,
        device_fingerprint: session_row_string(row, "device_fingerprint")?,
        trust_level: session_row_string(row, "trust_level")?,
        status: session_row_string(row, "status")?,
        last_monotonic_id: session_row_i64(row, "last_monotonic_id")?,
        expires_at: session_row_string(row, "expires_at")?,
    })
}

async fn load_device_response(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    device_id: &str,
) -> Result<DeviceLeaseResponse, (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT lease_id, session_id, device_id, device_fingerprint, trust_level,
                status, last_monotonic_id,
                DATE_FORMAT(expires_at, '%Y-%m-%dT%H:%i:%s') AS expires_at
         FROM session_device_leases
         WHERE user_id = ? AND session_id = ? AND device_id = ?
         LIMIT 1",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(device_id)
    .fetch_optional(pool.get())
    .await
    .map_err(internal_error)?
    .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "device lease not found"))?;
    decode_device_response(&row).map_err(internal_error)
}

fn decode_device_lease_row(row: &impl RowExt) -> Result<DeviceLeaseRow, String> {
    Ok(DeviceLeaseRow {
        lease_id: session_row_string(row, "lease_id")?,
        user_id: session_row_string(row, "user_id")?,
        session_id: session_row_string(row, "session_id")?,
        device_id: session_row_string(row, "device_id")?,
        device_fingerprint: session_row_string(row, "device_fingerprint")?,
        status: session_row_string(row, "status")?,
    })
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
    decode_device_lease_row(&row).map_err(|error| {
        internal_error(format!(
            "decode device lease revoke row failed for session {session_id}: {error}"
        ))
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
    rows.into_iter()
        .map(|row| decode_device_lease_event_payload(&row))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            internal_error(format!(
                "decode device lease event payload failed for session {session_id}: {error}"
            ))
        })
}

fn decode_device_lease_event_payload(row: &impl RowExt) -> Result<DeviceLeaseEndedPayload, String> {
    let event_type = session_row_string(row, "event_type")?;
    Ok(DeviceLeaseEndedPayload {
        r#type: if event_type == "auto_expire" {
            "device_lease_expired".to_string()
        } else {
            event_type
        },
        lease_id: session_row_string(row, "lease_id")?,
        session_id: session_row_string(row, "session_id")?,
        device_id: session_row_string(row, "device_id")?,
        device_fingerprint: session_row_string(row, "device_fingerprint")?,
        reason: session_row_string(row, "reason")?,
        ended_at_server: session_row_string(row, "ended_at_server")?,
    })
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
        StoredSessionArtifact,
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
    struct FakeSessionRow {
        failed_column: Option<&'static str>,
        i64_overrides: Vec<(&'static str, i64)>,
    }

    impl FakeSessionRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                i64_overrides: Vec::new(),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_i64(column: &'static str, value: i64) -> Self {
            Self {
                i64_overrides: vec![(column, value)],
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl RowExt for FakeSessionRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "lease_id" => "lease-1",
                "user_id" => "user-1",
                "session_id" => "session-1",
                "device_id" => "device-1",
                "device_fingerprint" => "fingerprint-1",
                "trust_level" => "trusted",
                "status" => "active",
                "role" => "assistant",
                "content" => "answer",
                "category" => "decision",
                "manifest_id" => "manifest-1",
                "turn_id" => "turn-1",
                "policy_version" => "v1",
                "created_at" => "2026-06-26T12:00:00",
                "expires_at" => "2026-06-26T14:00:00",
                "event_type" => "auto_expire",
                "reason" => "stale",
                "ended_at_server" => "2026-06-26T13:00:00",
                "page_hash" => "hash-1",
                "run_id" => "run-1",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "run_id" => Some("run-1".to_string()),
                "budget_template_id" => Some("budget-1".to_string()),
                "source_event_id" => None,
                "payload_json" => Some(
                    r#"{"reasoning":"thinking","reasoning_status":"complete","tool_calls":[{"tool_use_id":"call-1","name":"read_file","arguments":"{\"path\":\"src/lib.rs\"}"}]}"#
                        .to_string(),
                ),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            if let Some((_, value)) = self
                .i64_overrides
                .iter()
                .find(|(candidate, _)| *candidate == column)
            {
                return Ok(*value);
            }
            match column {
                "item_seq" => Ok(7),
                "last_monotonic_id" => Ok(42),
                "page_seq" => Ok(2),
                "start_item_seq" => Ok(5),
                "end_item_seq" => Ok(9),
                "item_count" => Ok(5),
                "page_count" => Ok(3),
                "page_high_watermark" => Ok(9),
                "projection_event_idx" => Ok(8),
                "total" => Ok(4),
                "total_estimated_tokens" => Ok(123),
                "high_watermark" => Ok(11),
                "last_event_idx" => Ok(10),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.fail_if_needed(column)?;
            match column {
                "meta_duration_ms" => Ok(Some(12)),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

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
        owned_session: Option<SessionRecord>,
    }

    impl RecordingSessionService {
        fn with_owned_session() -> Self {
            Self {
                get_session_calls: Mutex::new(Vec::new()),
                owned_session: Some(SessionRecord {
                    session_id: "placeholder-session".to_string(),
                    user_id: "placeholder-user".to_string(),
                    agent_id: None,
                    title: None,
                    metadata: serde_json::Map::new(),
                    status: "active".to_string(),
                    event_count: 0,
                    created_at: "2026-06-26T00:00:00".to_string(),
                    updated_at: None,
                    ended_at: None,
                }),
            }
        }
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
                .push((session_id.clone(), user_id.clone()));
            if let Some(record) = &self.owned_session {
                let mut record = record.clone();
                record.session_id = session_id;
                record.user_id = user_id;
                return Ok(record);
            }
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
            _cursor: Option<astra_services::auth::SessionActivityCursor>,
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
    async fn resume_session_does_not_activate_before_restore_is_available() {
        let session_service = Arc::new(RecordingSessionService::with_owned_session());
        let state = build_state(Arc::new(RecordingAuthService), session_service.clone());

        let error = resume_session_handler(
            State(state),
            Path("session-resume-order".to_string()),
            auth_headers(),
        )
        .await
        .expect_err("missing durable restore infrastructure must fail before activation");

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            session_service.get_session_calls.lock().await.as_slice(),
            &[(
                "session-resume-order".to_string(),
                "artifact-owner".to_string()
            )]
        );
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

    #[tokio::test]
    async fn session_state_and_transcript_handlers_verify_session_ownership_before_db_access() {
        let session_service = Arc::new(RecordingSessionService::default());
        let state = build_state(Arc::new(RecordingAuthService), session_service.clone());
        let session_id = "session-456".to_string();

        let state_err = match get_session_state_handler(
            State(state.clone()),
            Path(session_id.clone()),
            auth_headers(),
            Query(SessionStateQuery::default()),
        )
        .await
        {
            Ok(_) => panic!("state should stop at session ownership check"),
            Err(err) => err,
        };
        assert_eq!(state_err.0, StatusCode::FORBIDDEN);

        let transcript_err = match get_session_transcript_handler(
            State(state),
            Path(session_id.clone()),
            auth_headers(),
            Query(TranscriptQuery::default()),
        )
        .await
        {
            Ok(_) => panic!("transcript should stop at session ownership check"),
            Err(err) => err,
        };
        assert_eq!(transcript_err.0, StatusCode::FORBIDDEN);

        let calls = session_service.get_session_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![
                (session_id.clone(), "artifact-owner".to_string()),
                (session_id, "artifact-owner".to_string()),
            ],
            "state/transcript handlers should verify session ownership before touching durable session projections"
        );
    }

    #[test]
    fn device_protocol_identifiers_enforce_syntax_without_normalizing_identity() {
        let max = "a".repeat(DEVICE_PROTOCOL_VALUE_MAX_LEN);
        for accepted in ["device-1", "sha256:abc_DEF.123", max.as_str()] {
            assert_eq!(
                validate_device_protocol_value("device_id", accepted).unwrap(),
                accepted
            );
        }
        let too_long = "a".repeat(DEVICE_PROTOCOL_VALUE_MAX_LEN + 1);
        for rejected in [
            "",
            " device-1",
            "device-1 ",
            "device 1",
            "device\n1",
            "设备-1",
            too_long.as_str(),
        ] {
            let err = validate_device_protocol_value("device_id", rejected).unwrap_err();
            assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[test]
    fn session_state_requires_all_structured_device_proof_headers() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            (DEVICE_ID_HEADER, "device-1"),
            (DEVICE_FINGERPRINT_HEADER, "fp-1"),
            (DEVICE_CHALLENGE_ID_HEADER, "challenge-1"),
            (DEVICE_PROOF_HEADER, "proof-1"),
        ] {
            let err = required_device_header(&headers, name).unwrap_err();
            assert_eq!(err.0, StatusCode::UNAUTHORIZED);
            headers.insert(name, HeaderValue::from_static(value));
            assert_eq!(required_device_header(&headers, name).unwrap(), value);
        }
    }

    #[test]
    fn device_proof_is_bound_to_every_authority_dimension() {
        let key_hash = sha256_raw_hex(b"dk_test_secret");
        let challenge_digest = sha256_raw_hex(b"dc_test_nonce");
        let proof = "eynZMCSGx0fBYdE0b-maiJBHVaZgLBrmOOYV6j5CXYo";
        assert!(verify_device_proof_signature(
            &key_hash,
            DeviceProofPurpose::Hydrate,
            "user-17",
            "session:a",
            "laptop-2",
            "sha256:abcdef",
            "challenge-9",
            &challenge_digest,
            proof,
        ));

        let mutations = [
            (
                sha256_raw_hex(b"other-key"),
                DeviceProofPurpose::Hydrate,
                "user-17",
                "session:a",
                "laptop-2",
                "sha256:abcdef",
                "challenge-9",
                challenge_digest.clone(),
            ),
            (
                key_hash.clone(),
                DeviceProofPurpose::Trust,
                "user-17",
                "session:a",
                "laptop-2",
                "sha256:abcdef",
                "challenge-9",
                challenge_digest.clone(),
            ),
            (
                key_hash.clone(),
                DeviceProofPurpose::Hydrate,
                "user-18",
                "session:a",
                "laptop-2",
                "sha256:abcdef",
                "challenge-9",
                challenge_digest.clone(),
            ),
            (
                key_hash.clone(),
                DeviceProofPurpose::Hydrate,
                "user-17",
                "session:b",
                "laptop-2",
                "sha256:abcdef",
                "challenge-9",
                challenge_digest.clone(),
            ),
            (
                key_hash.clone(),
                DeviceProofPurpose::Hydrate,
                "user-17",
                "session:a",
                "laptop-3",
                "sha256:abcdef",
                "challenge-9",
                challenge_digest.clone(),
            ),
            (
                key_hash.clone(),
                DeviceProofPurpose::Hydrate,
                "user-17",
                "session:a",
                "laptop-2",
                "sha256:fedcba",
                "challenge-9",
                challenge_digest.clone(),
            ),
            (
                key_hash.clone(),
                DeviceProofPurpose::Hydrate,
                "user-17",
                "session:a",
                "laptop-2",
                "sha256:abcdef",
                "challenge-10",
                challenge_digest.clone(),
            ),
            (
                key_hash.clone(),
                DeviceProofPurpose::Hydrate,
                "user-17",
                "session:a",
                "laptop-2",
                "sha256:abcdef",
                "challenge-9",
                sha256_raw_hex(b"other-nonce"),
            ),
        ];
        for (key, purpose, user, session, device, fingerprint, challenge, digest) in mutations {
            assert!(
                !verify_device_proof_signature(
                    &key,
                    purpose,
                    user,
                    session,
                    device,
                    fingerprint,
                    challenge,
                    &digest,
                    proof,
                ),
                "mutating any bound authority dimension must invalidate the proof"
            );
        }
    }

    #[test]
    fn device_trust_request_rejects_boolean_self_attestation() {
        let result = serde_json::from_value::<DeviceTrustRequest>(serde_json::json!({
            "device_id": "device-1",
            "device_fingerprint": "fp-1",
            "challenge_id": "challenge-1",
            "device_proof": "proof",
            "reauthentication_proof": "reauth",
            "step_up_confirmation": true
        }));
        let error = match result {
            Ok(_) => panic!("legacy request-body self-attestation must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("step_up_confirmation"));
    }

    #[test]
    fn transcript_scope_keeps_root_conversation_and_run_reads_disjoint() {
        let root = TranscriptQuery {
            scope: Some(TranscriptQueryScope::RootConversation),
            ..TranscriptQuery::default()
        };
        assert_eq!(
            transcript_read_scope(&root).unwrap(),
            TranscriptReadScope::RootConversation
        );

        let run = TranscriptQuery {
            run_id: Some("run-1".to_string()),
            ..TranscriptQuery::default()
        };
        assert_eq!(
            transcript_read_scope(&run).unwrap(),
            TranscriptReadScope::Run("run-1")
        );

        let ambiguous = TranscriptQuery {
            run_id: Some("run-1".to_string()),
            scope: Some(TranscriptQueryScope::RootConversation),
            ..TranscriptQuery::default()
        };
        let error = transcript_read_scope(&ambiguous).unwrap_err();
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn transcript_item_decode_preserves_structured_durable_payload() {
        let item =
            decode_transcript_item(&FakeSessionRow::complete()).expect("complete row decodes");

        assert_eq!(item.session_id, "session-1");
        assert_eq!(item.item_seq, 7);
        assert_eq!(item.run_id.as_deref(), Some("run-1"));
        assert_eq!(item.role, "assistant");
        assert_eq!(item.content, "answer");
        assert_eq!(item.reasoning.as_deref(), Some("thinking"));
        assert_eq!(item.reasoning_status.as_deref(), Some("complete"));
        assert_eq!(item.tool_calls.len(), 1);
        assert_eq!(item.tool_calls[0].tool_use_id, "call-1");
        assert_eq!(item.tool_calls[0].name, "read_file");
        assert_eq!(item.created_at, "2026-06-26T12:00:00");
    }

    #[test]
    fn transcript_item_decode_fails_loudly_on_required_columns() {
        for column in ["session_id", "item_seq", "role", "content", "created_at"] {
            let err = match decode_transcript_item(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing transcript item column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify missing transcript column {column}: {err}"
            );
        }
    }

    #[test]
    fn transcript_page_ref_decode_fails_loudly_on_required_columns() {
        let page = decode_transcript_page_ref(&FakeSessionRow::complete())
            .expect("complete page ref decodes");
        assert_eq!(page.page_seq, 2);
        assert_eq!(page.start_item_seq, 5);
        assert_eq!(page.end_item_seq, 9);
        assert_eq!(page.item_count, 5);
        assert_eq!(page.page_hash, "hash-1");

        for column in [
            "page_seq",
            "start_item_seq",
            "end_item_seq",
            "item_count",
            "page_hash",
        ] {
            let err = match decode_transcript_page_ref(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing transcript page column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify missing page column {column}: {err}"
            );
        }
    }

    #[test]
    fn transcript_page_stats_decode_fails_loudly_on_required_columns() {
        let (page_count, high_watermark) =
            decode_transcript_page_stats(&FakeSessionRow::complete()).expect("stats decode");
        assert_eq!(page_count, 3);
        assert_eq!(high_watermark, 9);

        for column in ["page_count", "page_high_watermark"] {
            let err = match decode_transcript_page_stats(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing transcript stats column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify stats column {column}: {err}"
            );
        }

        for (column, value) in [
            ("page_count", -1),
            ("page_count", i64::from(u32::MAX) + 1),
            ("page_high_watermark", -1),
        ] {
            let err = decode_transcript_page_stats(&FakeSessionRow::with_i64(column, value))
                .expect_err("invalid transcript stats value must fail");
            assert!(
                err.contains(column),
                "error should identify invalid stats column {column}: {err}"
            );
        }
    }

    #[test]
    fn state_summary_decode_preserves_values_and_fails_loudly() {
        let summary = decode_state_category_summary(&FakeSessionRow::complete())
            .expect("state summary decodes");
        assert_eq!(summary.category, "decision");
        assert_eq!(summary.count, 4);

        for column in ["category", "total"] {
            let err = match decode_state_category_summary(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing state summary column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify state summary column {column}: {err}"
            );
        }
        for value in [-1, i64::from(u32::MAX) + 1] {
            let err = decode_state_category_summary(&FakeSessionRow::with_i64("total", value))
                .expect_err("invalid state summary count must fail");
            assert!(err.contains("total"), "error should identify total: {err}");
        }
    }

    #[test]
    fn context_manifest_summary_decode_preserves_values_and_fails_loudly() {
        let summary = decode_context_manifest_summary(&FakeSessionRow::complete())
            .expect("context manifest decodes");
        assert_eq!(summary.manifest_id, "manifest-1");
        assert_eq!(summary.run_id.as_deref(), Some("run-1"));
        assert_eq!(summary.turn_id, "turn-1");
        assert_eq!(summary.reason, "stale");
        assert_eq!(summary.total_estimated_tokens, 123);
        assert_eq!(summary.budget_template_id.as_deref(), Some("budget-1"));
        assert_eq!(summary.policy_version, "v1");
        assert_eq!(summary.created_at, "2026-06-26T12:00:00");

        for column in [
            "manifest_id",
            "run_id",
            "turn_id",
            "reason",
            "total_estimated_tokens",
            "budget_template_id",
            "policy_version",
            "created_at",
        ] {
            let err = match decode_context_manifest_summary(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing context manifest column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify context manifest column {column}: {err}"
            );
        }
        for value in [-1, i64::from(u32::MAX) + 1] {
            let err = decode_context_manifest_summary(&FakeSessionRow::with_i64(
                "total_estimated_tokens",
                value,
            ))
            .expect_err("invalid token count must fail");
            assert!(
                err.contains("total_estimated_tokens"),
                "error should identify token column: {err}"
            );
        }
    }

    #[test]
    fn active_run_projection_decode_preserves_values_and_fails_loudly() {
        let active =
            decode_active_run_projection(&FakeSessionRow::complete()).expect("active run decodes");
        assert_eq!(active.run_id, "run-1");
        assert_eq!(active.run_event_high_watermark, 10);
        assert!(!active.replay_required);
        assert_eq!(active.replay_start_event_idx, 0);

        let empty_run =
            decode_active_run_projection(&FakeSessionRow::with_i64("last_event_idx", -1))
                .expect("last_event_idx -1 means no run events yet");
        assert_eq!(empty_run.run_event_high_watermark, 0);

        for column in ["run_id", "last_event_idx"] {
            let err = match decode_active_run_projection(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing active run column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify active run column {column}: {err}"
            );
        }
        let err =
            match decode_active_run_projection(&FakeSessionRow::with_i64("last_event_idx", -2)) {
                Ok(_) => panic!("last_event_idx less than -1 must fail"),
                Err(err) => err,
            };
        assert!(
            err.contains("last_event_idx"),
            "error should identify invalid last_event_idx: {err}"
        );
    }

    #[test]
    fn device_response_decode_preserves_values_and_fails_loudly() {
        let device = decode_device_response(&FakeSessionRow::complete())
            .expect("complete device row decodes");
        assert_eq!(device.lease_id, "lease-1");
        assert_eq!(device.session_id, "session-1");
        assert_eq!(device.device_id, "device-1");
        assert_eq!(device.device_fingerprint, "fingerprint-1");
        assert_eq!(device.trust_level, "trusted");
        assert_eq!(device.status, "active");
        assert_eq!(device.last_monotonic_id, 42);
        assert_eq!(device.expires_at, "2026-06-26T14:00:00");

        for column in [
            "lease_id",
            "session_id",
            "device_id",
            "device_fingerprint",
            "trust_level",
            "status",
            "last_monotonic_id",
            "expires_at",
        ] {
            let err = match decode_device_response(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing device response column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify missing device response column {column}: {err}"
            );
        }
    }

    #[test]
    fn device_lease_row_decode_preserves_owner_and_fails_loudly() {
        let lease =
            decode_device_lease_row(&FakeSessionRow::complete()).expect("lease row decodes");
        assert_eq!(lease.lease_id, "lease-1");
        assert_eq!(lease.user_id, "user-1");
        assert_eq!(lease.session_id, "session-1");
        assert_eq!(lease.device_id, "device-1");
        assert_eq!(lease.device_fingerprint, "fingerprint-1");
        assert_eq!(lease.status, "active");

        for column in [
            "lease_id",
            "user_id",
            "session_id",
            "device_id",
            "device_fingerprint",
            "status",
        ] {
            let err = match decode_device_lease_row(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing device lease row column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify missing device lease row column {column}: {err}"
            );
        }
    }

    #[test]
    fn device_lease_event_payload_decode_maps_auto_expire_and_fails_loudly() {
        let payload = decode_device_lease_event_payload(&FakeSessionRow::complete())
            .expect("event payload decodes");
        assert_eq!(payload.r#type, "device_lease_expired");
        assert_eq!(payload.lease_id, "lease-1");
        assert_eq!(payload.session_id, "session-1");
        assert_eq!(payload.device_id, "device-1");
        assert_eq!(payload.device_fingerprint, "fingerprint-1");
        assert_eq!(payload.reason, "stale");
        assert_eq!(payload.ended_at_server, "2026-06-26T13:00:00");

        for column in [
            "event_type",
            "lease_id",
            "session_id",
            "device_id",
            "device_fingerprint",
            "reason",
            "ended_at_server",
        ] {
            let err = match decode_device_lease_event_payload(&FakeSessionRow::fail_on(column)) {
                Ok(_) => panic!("missing device lease event column must fail: {column}"),
                Err(err) => err,
            };
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify missing lease event column {column}: {err}"
            );
        }
    }

    #[test]
    fn workspace_authority_reads_workspace_metadata_artifact() {
        let artifact = StoredSessionArtifact {
            artifact_id: "artifact-1".to_string(),
            session_id: "session-1".to_string(),
            user_id: "user-1".to_string(),
            artifact_kind: WORKSPACE_METADATA_ARTIFACT_KIND.to_string(),
            source: Some("workspace_metadata".to_string()),
            turn: Some(3),
            round: None,
            content: json!({
                "session_id": "session-1",
                "cwd": "/tmp/project",
                "git_root": "/tmp/project",
                "git_branch": "main",
                "git_head": "abc123",
                "model": "gpt-5.4",
                "created_at": "2026-05-18T00:00:00Z",
                "updated_at": "2026-05-18T00:01:00Z",
                "turn_count": 3,
                "total_tokens_in": 10,
                "total_tokens_out": 20,
                "status": "active",
                "checkpoints": []
            }),
            metadata: None,
            retention_policy: None,
            retention_until: None,
            status: Some("active".to_string()),
            referenced_by_manifest_count: 0,
            referenced_by_state_items_count: 0,
            referenced_by_citation_count: 0,
            referenced_by_durable_count: 0,
            created_at: Some("2026-05-18T00:01:00Z".to_string()),
        };

        let authority =
            workspace_authority_from_artifact(&artifact).expect("workspace metadata should decode");

        assert_eq!(authority.cwd, "/tmp/project");
        assert_eq!(authority.git_branch.as_deref(), Some("main"));
        assert_eq!(authority.git_head.as_deref(), Some("abc123"));
        assert_eq!(authority.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(authority.updated_at, "2026-05-18T00:01:00Z");
    }
}
