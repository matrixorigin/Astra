//! `GET /service/edges/status?user_id=xxx` — service-to-service edge status.
//!
//! Allows trusted backend services (moi-backend) to query live edge connection
//! status for any user without needing a user session token. Auth is a static
//! shared secret configured via ASTRA_BACKEND_SERVICE_KEY environment variable.

use super::*;

const SERVICE_KEY_ENV: &str = "ASTRA_BACKEND_SERVICE_KEY";

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn check_service_auth(headers: &HeaderMap) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let expected = match std::env::var(SERVICE_KEY_ENV) {
        Ok(k) if !k.is_empty() => k,
        _ => {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "service endpoint not configured (ASTRA_BACKEND_SERVICE_KEY not set)",
            ));
        }
    };
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid service key",
        ));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
pub(crate) struct ServiceEdgeStatusQuery {
    pub user_id: String,
}

/// Heartbeat TTL: DB entries whose `last_heartbeat_at` is older than this are
/// treated as stale (edge likely disconnected from another pod).
/// Must be >= 3× EDGE_HEARTBEAT_INTERVAL_SECS (30s) to tolerate tick jitter
/// and the MissedTickBehavior::Delay policy without false disconnects.
const HEARTBEAT_TTL_SECS: i64 = 90;

pub(crate) async fn edge_service_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ServiceEdgeStatusQuery>,
) -> Result<Json<edge_status_handler::EdgeStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    check_service_auth(&headers)?;

    if query.user_id.trim().is_empty() {
        return Err(error_response(StatusCode::BAD_REQUEST, "user_id required"));
    }

    // Build in-memory pool lookup for connected_secs enrichment (this-pod connections).
    let in_memory_edges = state.edge_connection_pool.get_user_edges(&query.user_id);
    let in_memory: std::collections::HashMap<String, std::time::Instant> = in_memory_edges
        .into_iter()
        .map(|info| (info.edge_agent_id.clone(), info.connected_at))
        .collect();

    // Primary: query DB registry for cross-pod correctness.
    // The DB registry is the authoritative source for multi-pod clusters;
    // falling back to the in-memory pool would return incomplete data and
    // silently mislead callers. Return 503 on failure so the client can
    // distinguish between "no edges" and "status unavailable".
    let db_records = match state
        .execution
        .edge_registry_service
        .list_by_user(&query.user_id)
        .await
    {
        Ok(records) => records,
        Err(e) => {
            tracing::warn!(
                target: "astra_runtime::edge_service_status",
                user_id = %query.user_id,
                error = %e,
                "edge_registry list_by_user failed; returning 503 to signal degraded state"
            );
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "edge registry unavailable",
            ));
        }
    };

    // When no DB records are found (unconfigured / single-pod deployment),
    // synthesise records from the in-memory pool so that callers can see
    // locally connected edges even without a DB registry.
    let now = Utc::now();
    if db_records.is_empty() && !in_memory.is_empty() {
        let edges: Vec<edge_status_handler::EdgeInfo> = state
            .edge_connection_pool
            .get_user_edges(&query.user_id)
            .into_iter()
            .map(|info| edge_status_handler::EdgeInfo {
                connected_secs: info.connected_at.elapsed().as_secs(),
                capabilities: info.capabilities,
                edge_agent_id: info.edge_agent_id,
                hostname: info.hostname,
                workspace_dir: info.workspace_dir,
            })
            .collect();
        return Ok(Json(edge_status_handler::EdgeStatusResponse { edges }));
    }

    let edges: Vec<edge_status_handler::EdgeInfo> = db_records
        .into_iter()
        .filter_map(|record| {
            // Apply heartbeat TTL: discard entries whose last heartbeat is too old.
            // The format from `CAST(last_heartbeat_at AS CHAR)` is "%Y-%m-%d %H:%M:%S%.6f".
            let heartbeat_naive = chrono::NaiveDateTime::parse_from_str(
                &record.last_heartbeat_at,
                "%Y-%m-%d %H:%M:%S%.6f",
            )
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(
                    &record.last_heartbeat_at,
                    "%Y-%m-%d %H:%M:%S",
                )
            })
            .ok()?;
            let heartbeat_at = heartbeat_naive.and_utc();
            let age_secs = (now - heartbeat_at).num_seconds();
            if age_secs > HEARTBEAT_TTL_SECS {
                tracing::debug!(
                    target: "astra_runtime::edge_service_status",
                    edge_agent_id = %record.edge_agent_id,
                    age_secs,
                    "edge_registry: discarding stale entry"
                );
                return None;
            }

            // Use in-memory connected_at if the edge is also locally tracked.
            let connected_secs = in_memory
                .get(&record.edge_agent_id)
                .map(|connected_at| connected_at.elapsed().as_secs())
                .unwrap_or(0);

            Some(edge_status_handler::EdgeInfo {
                edge_agent_id: record.edge_agent_id,
                hostname: record.hostname,
                workspace_dir: record.worktree_path,
                capabilities: record.capabilities,
                connected_secs,
            })
        })
        .collect();

    Ok(Json(edge_status_handler::EdgeStatusResponse { edges }))
}
