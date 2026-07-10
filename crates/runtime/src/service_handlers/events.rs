use astra_services::events::*;

use crate::AppState;
use astra_core::ErrorResponse;
use astra_sync_protocol::{
    SYNC_OUTBOX_SIGNATURE_HEADER, SyncOutboxAck, SyncOutboxIngestionStatus,
    sync_outbox_request_signature,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
};
use serde_json::Value;

pub async fn create_event_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<EventCreateRequest>,
) -> Result<(StatusCode, Json<EventResponse>), (StatusCode, Json<ErrorResponse>)> {
    create_event_with_source(state, headers, request, EventIngestionSource::Client).await
}

pub async fn create_sync_outbox_event_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<SyncOutboxAck>), (StatusCode, Json<ErrorResponse>)> {
    require_sync_outbox_signature(&headers, &body)?;
    let request = serde_json::from_value::<EventCreateRequest>(body).map_err(|error| {
        astra_core::error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid sync outbox event request: {error}"),
        )
    })?;
    let outcome =
        create_event_outcome(state, headers, request, EventIngestionSource::SyncOutbox).await?;
    let (status, ack) = sync_outbox_ack_from_outcome(outcome)?;
    Ok((status, Json(ack)))
}

fn sync_outbox_ack_from_outcome(
    outcome: EventCreateOutcome,
) -> Result<(StatusCode, SyncOutboxAck), (StatusCode, Json<ErrorResponse>)> {
    let payload_hash = outcome
        .record
        .metadata
        .get("sync_outbox")
        .and_then(|sync| sync.get("payload_hash"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            astra_core::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Stored sync outbox event is missing its payload hash",
            )
        })?;
    let status = if outcome.idempotent_replay {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let ingestion_status = if outcome.idempotent_replay {
        SyncOutboxIngestionStatus::IdempotentReplay
    } else {
        SyncOutboxIngestionStatus::Created
    };
    Ok((
        status,
        SyncOutboxAck::new(outcome.record.event_id, payload_hash, ingestion_status),
    ))
}

async fn create_event_with_source(
    state: AppState,
    headers: HeaderMap,
    request: EventCreateRequest,
    ingestion_source: EventIngestionSource,
) -> Result<(StatusCode, Json<EventResponse>), (StatusCode, Json<ErrorResponse>)> {
    let outcome = create_event_outcome(state, headers, request, ingestion_source).await?;
    let status = if outcome.idempotent_replay {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(EventResponse::from(outcome.record))))
}

async fn create_event_outcome(
    state: AppState,
    headers: HeaderMap,
    request: EventCreateRequest,
    ingestion_source: EventIngestionSource,
) -> Result<EventCreateOutcome, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let outcome = state
        .event_service
        .create_event(
            user.user_id,
            EventCreateRequestData {
                ingestion_source,
                event_id: request.event_id,
                session_id: request.session_id,
                event_type: request.event_type,
                content: request.content,
                agent_id: request.agent_id,
                agent_version: request.agent_version,
                parent_event_id: request.parent_event_id,
                parent_event_ids: request.parent_event_ids,
                causal_chain_id: request.causal_chain_id,
                metadata: request.metadata,
            },
        )
        .await?;
    Ok(outcome)
}

fn require_sync_outbox_signature(
    headers: &HeaderMap,
    body: &Value,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let token = bearer_token(headers)?;
    let expected = sync_outbox_request_signature(token, body);
    let actual = headers
        .get(SYNC_OUTBOX_SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if constant_time_eq(expected.as_bytes(), actual.as_bytes()) {
        Ok(())
    } else {
        Err(astra_core::error_response(
            StatusCode::UNAUTHORIZED,
            "Invalid sync outbox request signature",
        ))
    }
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, (StatusCode, Json<ErrorResponse>)> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| astra_core::error_response(StatusCode::UNAUTHORIZED, "Missing token"))?;
    value
        .strip_prefix("Bearer ")
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| astra_core::error_response(StatusCode::UNAUTHORIZED, "Invalid token"))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

pub async fn list_events_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventListQuery>,
) -> Result<Json<EventListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let cursor = q.cursor()?;
    let list = state
        .event_service
        .list_events(EventListFilter {
            user_id: user.user_id,
            session_id: q.session_id,
            event_type: q.event_type,
            agent_id: q.agent_id,
            causal_chain_id: q.causal_chain_id,
            limit: q.limit,
            cursor,
        })
        .await?;
    Ok(Json(EventListResponse::from(list)))
}

pub async fn get_event_handler(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<EventResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let event = state
        .event_service
        .get_event(event_id, user.user_id)
        .await?;
    Ok(Json(EventResponse::from(event)))
}

pub async fn get_causal_chain_handler(
    State(state): State<AppState>,
    Path(causal_chain_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<EventResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let events = state
        .event_service
        .get_causal_chain(causal_chain_id, user.user_id)
        .await?;
    Ok(Json(events.into_iter().map(EventResponse::from).collect()))
}

pub async fn get_session_events_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<SessionEventQuery>,
) -> Result<Json<EventListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let cursor = q.cursor()?;
    let list = state
        .event_service
        .get_session_events(session_id, user.user_id, q.limit, cursor)
        .await?;
    Ok(Json(EventListResponse::from(list)))
}

pub async fn delete_event_handler(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .event_service
        .delete_event(event_id, user.user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn signed_sync_outbox_headers(token: &str, body: &serde_json::Value) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers.insert(
            SYNC_OUTBOX_SIGNATURE_HEADER,
            HeaderValue::from_str(&sync_outbox_request_signature(token, body)).unwrap(),
        );
        headers
    }

    #[test]
    fn sync_outbox_signature_accepts_canonical_body() {
        let body = serde_json::json!({"b":2,"a":1});
        let equivalent_body = serde_json::json!({"a":1,"b":2});
        let headers = signed_sync_outbox_headers("tok", &body);

        assert!(require_sync_outbox_signature(&headers, &equivalent_body).is_ok());
    }

    #[test]
    fn sync_outbox_signature_rejects_body_mismatch() {
        let signed_body = serde_json::json!({"event_id":"a","content":"one"});
        let received_body = serde_json::json!({"event_id":"a","content":"two"});
        let headers = signed_sync_outbox_headers("tok", &signed_body);

        assert!(matches!(
            require_sync_outbox_signature(&headers, &received_body),
            Err((StatusCode::UNAUTHORIZED, _))
        ));
    }

    #[test]
    fn sync_outbox_ack_is_explicit_for_created_and_replayed_events() {
        fn record() -> EventRecord {
            EventRecord {
                event_id: "record-1".to_string(),
                user_id: "user-1".to_string(),
                session_id: "session-1".to_string(),
                event_type: "sync_marker".to_string(),
                content: "{}".to_string(),
                agent_id: Some("edge_sync".to_string()),
                agent_version: Some("test".to_string()),
                parent_event_id: None,
                parent_event_ids: Vec::new(),
                causal_chain_id: None,
                metadata: serde_json::json!({
                    "sync_outbox": {"payload_hash": "sha256:abc"}
                }),
                created_at: "2026-07-10T00:00:00Z".to_string(),
            }
        }

        let (created_status, created_ack) =
            sync_outbox_ack_from_outcome(EventCreateOutcome::created(record())).unwrap();
        assert_eq!(created_status, StatusCode::CREATED);
        assert!(created_ack.confirms("record-1", "sha256:abc"));
        assert_eq!(
            created_ack.ingestion_status,
            SyncOutboxIngestionStatus::Created
        );

        let (replay_status, replay_ack) =
            sync_outbox_ack_from_outcome(EventCreateOutcome::replayed(record())).unwrap();
        assert_eq!(replay_status, StatusCode::OK);
        assert_eq!(
            replay_ack.ingestion_status,
            SyncOutboxIngestionStatus::IdempotentReplay
        );
    }
}
