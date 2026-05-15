//! Cloud REST client for plan-mode tool calls.
//!
//! Astra is edge-cloud: plan state lives in the cloud `plans` table and is
//! authoritative across devices. The CLI plan-mode tool wrappers call into
//! this module so they don't reach into ThinClient internals or duplicate
//! HTTP plumbing.
//!
//! Endpoints exercised:
//! * `POST /plans` — create a fresh plan and link it to the session.
//! * `POST /plans/{plan_id}/exit-plan-mode` — present plan for approval and
//!   (server-side) seed `session_plan_todos` on `approved=true`.
//!
//! The `enter_plan_mode` flow does NOT need a session→plan lookup; the
//! server's `set_active_plan` makes the new plan the session's active
//! one atomically. `exit_plan_mode` resolves the active plan via the
//! plan_id field in the success path.

use serde::Deserialize;
use serde_json::json;

const PLAN_MODE_HTTP_TIMEOUT_SECS: u64 = 15;

#[derive(Deserialize)]
struct CreatePlanResponse {
    plan_id: String,
}

#[derive(Deserialize)]
struct PlanModeResponse {
    plan_id: String,
}

/// Build a `reqwest::Client` with the auth header set if a token is
/// available. The cloud server's `current_user` extractor reads
/// `Authorization: Bearer ...`; without it the call returns 401.
fn build_request(
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
) -> Result<reqwest::RequestBuilder, String> {
    let client = reqwest::Client::builder()
        .no_proxy() // astra server is local/intranet; bypass http_proxy env
        .timeout(std::time::Duration::from_secs(PLAN_MODE_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("http client init: {e}"))?;
    let mut req = client.request(method, url);
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    Ok(req)
}

/// `POST /plans` — mint a new plan, link it to the session as active, return
/// the cloud-assigned plan_id. The session id is required so the cloud's
/// `active_plan_id` is set atomically.
pub async fn enter_plan_mode(
    cloud_base: &str,
    token: Option<&str>,
    session_id: &str,
    goal: &str,
) -> Result<String, String> {
    let url = format!("{}/plans", cloud_base.trim_end_matches('/'));
    let body = json!({
        "goal": goal,
        "session_id": session_id,
    });
    let resp = build_request(reqwest::Method::POST, &url, token)?
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("cloud {status}: {body}"));
    }
    let parsed: CreatePlanResponse = resp
        .json()
        .await
        .map_err(|e| format!("decode response: {e}"))?;
    Ok(parsed.plan_id)
}

/// `POST /plans/{id}/exit-plan-mode` — surface the plan markdown for user
/// approval and (on `approved=true`) flip the session out of plan mode. The
/// server-side handler also seeds `session_plan_todos` from the plan's
/// subtasks so the next turn can execute step-by-step.
///
/// Resolves the active plan via the cloud `GET /plans?session_id=...` —
/// without this the CLI would need to track plan_id locally and the two
/// could desync (CLI restart drops the in-memory id, cloud still has the
/// active plan pinned).
pub async fn exit_plan_mode(
    cloud_base: &str,
    token: Option<&str>,
    session_id: &str,
    plan_md: &str,
    approved: bool,
) -> Result<String, String> {
    let base = cloud_base.trim_end_matches('/');
    // Look up the session's active plan. We trust the cloud's
    // `active_plan_id` over any locally-held value because session
    // restart, multi-device use, and tool-failure recovery can all
    // leave a stale id on the edge.
    let list_url = format!("{base}/plans?session_id={session_id}&phase=planning");
    let list_resp = build_request(reqwest::Method::GET, &list_url, token)?
        .send()
        .await
        .map_err(|e| format!("network (list plans): {e}"))?;
    if !list_resp.status().is_success() {
        return Err(format!(
            "cloud {} listing plans: {}",
            list_resp.status(),
            list_resp.text().await.unwrap_or_default()
        ));
    }
    let plans_value: serde_json::Value = list_resp
        .json()
        .await
        .map_err(|e| format!("decode list plans: {e}"))?;
    let plan_id = plans_value
        .get("plans")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find_map(|p| p.get("plan_id").and_then(|v| v.as_str()))
        })
        .ok_or_else(|| "no active plan for this session — call enter_plan_mode first".to_string())?
        .to_string();

    let exit_url = format!("{base}/plans/{plan_id}/exit-plan-mode");
    let body = json!({
        "approved": approved,
        "plan_md": plan_md,
    });
    let resp = build_request(reqwest::Method::POST, &exit_url, token)?
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("network (exit plan): {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("cloud {status} exiting plan mode: {body}"));
    }
    // Server returns PlanResponse with plan_id; we re-confirm so callers
    // see the same id end-to-end.
    let parsed: PlanModeResponse = resp
        .json()
        .await
        .map_err(|e| format!("decode exit response: {e}"))?;
    Ok(parsed.plan_id)
}
