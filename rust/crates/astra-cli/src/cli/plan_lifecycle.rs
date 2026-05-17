use astra_runtime::plan;
use serde_json::Value;

use crate::session_journal;
use crate::session_runtime;

fn parse_session_id(value: &Value) -> Option<String> {
    value
        .get("session_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .filter(|sid| !sid.trim().is_empty())
        .map(ToString::to_string)
}

fn parse_plan_id(value: &Value) -> Option<String> {
    value
        .get("plan_id")
        .and_then(Value::as_str)
        .filter(|plan_id| !plan_id.trim().is_empty())
        .map(ToString::to_string)
}

fn build_plan_mode_state(
    goal: String,
    plan_json: Option<Value>,
    version: Option<u64>,
) -> plan::PlanModeState {
    let mut state = plan::PlanModeState::new(goal);
    if let Some(plan_json) = plan_json
        && let Ok(task_plan) =
            serde_json::from_value::<astra_services::task_orchestrator::TaskPlan>(plan_json)
    {
        state.plan = task_plan;
        state.modified = true;
    }
    if let Some(version) = version {
        state.version = version;
    }
    state
}

fn bind_state_to_session(state: &mut crate::SessionState, profile: Option<&str>, session_id: &str) {
    if state.session_id.as_deref() == Some(session_id) {
        return;
    }
    let _ = crate::persist_profile_last_session(profile, session_id);
    state.task_manager.rebind(session_id);
    state.session_id = Some(session_id.to_string());
    if state.journal.is_none() {
        state.journal = session_journal::JournalWriter::new(session_id).ok();
    }
}

async fn ensure_cloud_session(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: &str,
    state: &mut crate::SessionState,
) -> Result<String, String> {
    if let Some(session_id) = state
        .session_id
        .as_ref()
        .filter(|sid| !sid.trim().is_empty())
    {
        return Ok(session_id.clone());
    }
    let value = serde_json::from_str::<Value>(
        &api.post_sessions_json(token, &serde_json::json!({}))
            .await
            .map_err(crate::map_thin_err)?,
    )
    .unwrap_or_default();
    let session_id = parse_session_id(&value)
        .ok_or_else(|| "session create response missing session_id".to_string())?;
    bind_state_to_session(state, profile, &session_id);
    Ok(session_id)
}

pub(crate) async fn enter_remote_plan_mode(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: &str,
    state: &mut crate::SessionState,
    goal: &str,
) -> Result<String, String> {
    let goal = goal.trim();
    if goal.is_empty() {
        return Err("plan goal must be non-empty".to_string());
    }
    let session_id = ensure_cloud_session(api, profile, token, state).await?;
    let value = api
        .post_plans_json(
            token,
            &serde_json::json!({
                "goal": goal,
                "session_id": session_id,
            }),
        )
        .await
        .map_err(crate::map_thin_err)?;
    let plan_id =
        parse_plan_id(&value).ok_or_else(|| "create plan response missing plan_id".to_string())?;
    state.plan_mode = Some(build_plan_mode_state(
        goal.to_string(),
        value.get("plan").cloned(),
        value.get("version").and_then(Value::as_u64),
    ));
    state.plan_mode_sync_error = None;
    Ok(plan_id)
}

pub(crate) async fn active_remote_planning_plan_id(
    api: &astra_thin_client::ThinClient,
    token: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    let plans = api
        .get_plans_query_json(
            token,
            &[
                ("session_id", session_id.to_string()),
                ("phase", "planning".to_string()),
                ("limit", "1".to_string()),
            ],
        )
        .await
        .map_err(crate::map_thin_err)?;

    Ok(plans
        .get("plans")
        .and_then(Value::as_array)
        .and_then(|plans| plans.first())
        .and_then(parse_plan_id))
}

pub(crate) async fn exit_remote_plan_mode(
    api: &astra_thin_client::ThinClient,
    token: &str,
    state: &mut crate::SessionState,
    approved: bool,
) -> Result<Option<String>, String> {
    let Some(session_id) = state
        .session_id
        .as_ref()
        .filter(|sid| !sid.trim().is_empty())
    else {
        state.plan_mode = None;
        state.plan_mode_sync_error = None;
        return Ok(None);
    };

    let Some(plan_id) = active_remote_planning_plan_id(api, token, session_id).await? else {
        state.plan_mode = None;
        state.plan_mode_sync_error = None;
        return Ok(None);
    };

    let response = api
        .post_plan_exit_mode_json(
            token,
            &plan_id,
            &serde_json::json!({ "approved": approved }),
        )
        .await
        .map_err(crate::map_thin_err)?;

    if approved {
        state.plan_mode = None;
        state.plan_mode_sync_error = None;
    } else {
        sync_remote_plan_mode_state(api, token, state).await?;
    }

    Ok(response
        .get("plan_id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or(Some(plan_id)))
}

pub(crate) async fn sync_remote_plan_mode_state(
    api: &astra_thin_client::ThinClient,
    token: &str,
    state: &mut crate::SessionState,
) -> Result<(), String> {
    let Some(session_id) = state
        .session_id
        .as_ref()
        .filter(|sid| !sid.trim().is_empty())
    else {
        state.plan_mode = None;
        state.plan_mode_sync_error = None;
        return Ok(());
    };

    let Some(plan_id) = active_remote_planning_plan_id(api, token, session_id).await? else {
        state.plan_mode = None;
        state.plan_mode_sync_error = None;
        return Ok(());
    };

    let plan_state = api
        .get_plan_json(token, &plan_id)
        .await
        .map_err(crate::map_thin_err)?;
    let goal = plan_state
        .get("goal")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    state.plan_mode = Some(build_plan_mode_state(
        goal,
        plan_state.get("plan").cloned(),
        plan_state.get("version").and_then(Value::as_u64),
    ));
    state.plan_mode_sync_error = None;
    Ok(())
}

pub(crate) async fn fresh_token_for_plan(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Option<String> {
    session_runtime::fresh_access_token(api, profile).await
}

pub(crate) fn looks_like_pending_local_plan_entry(state: &crate::SessionState) -> bool {
    state
        .plan_mode
        .as_ref()
        .map(|plan| plan.goal.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn enter_remote_plan_mode_creates_session_when_missing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "sess-1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_id": "plan-1",
                "goal": "Ship auth",
                "version": 4,
                "plan": {
                    "subtasks": [
                        {
                            "id": "s1",
                            "title": "Inspect auth flow",
                            "description": null,
                            "depends_on": [],
                            "status": "pending"
                        }
                    ]
                }
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();
        let plan_id = enter_remote_plan_mode(&api, None, "token", &mut state, "Ship auth")
            .await
            .unwrap();

        assert_eq!(plan_id, "plan-1");
        assert_eq!(state.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            state.plan_mode.as_ref().map(|plan| plan.goal.as_str()),
            Some("Ship auth")
        );
        assert_eq!(
            state
                .plan_mode
                .as_ref()
                .map(|plan| plan.plan.subtasks.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn enter_remote_plan_mode_keeps_bound_session_when_plan_create_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "sess-1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "detail": "temporarily unavailable"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();

        let error = enter_remote_plan_mode(&api, None, "token", &mut state, "Ship auth")
            .await
            .expect_err("plan create should fail");

        assert!(error.contains("503"), "got: {error}");
        assert_eq!(state.session_id.as_deref(), Some("sess-1"));
        assert!(
            state.plan_mode.is_none(),
            "failed plan create must not arm local mirror"
        );
    }

    #[tokio::test]
    async fn sync_remote_plan_mode_state_clears_when_no_planning_plan_exists() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("phase", "planning"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plans": []
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();
        state.session_id = Some("sess-1".to_string());
        state.plan_mode = Some(plan::PlanModeState::new("stale".to_string()));

        sync_remote_plan_mode_state(&api, "token", &mut state)
            .await
            .unwrap();
        assert!(state.plan_mode.is_none());
    }

    #[tokio::test]
    async fn sync_remote_plan_mode_state_loads_latest_planning_plan() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("phase", "planning"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plans": [
                    { "plan_id": "plan-9", "goal": "Ship auth" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/plans/plan-9"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_id": "plan-9",
                "goal": "Ship auth",
                "version": 7,
                "plan": {
                    "subtasks": [
                        {
                            "id": "s1",
                            "title": "Inspect auth flow",
                            "description": null,
                            "depends_on": [],
                            "status": "pending"
                        },
                        {
                            "id": "s2",
                            "title": "Write plan doc",
                            "description": null,
                            "depends_on": ["s1"],
                            "status": "pending"
                        }
                    ]
                }
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();
        state.session_id = Some("sess-1".to_string());

        sync_remote_plan_mode_state(&api, "token", &mut state)
            .await
            .unwrap();

        let plan = state.plan_mode.expect("planning plan should be mirrored");
        assert_eq!(plan.goal, "Ship auth");
        assert_eq!(plan.version, 7);
        assert_eq!(plan.plan.subtasks.len(), 2);
    }

    #[tokio::test]
    async fn sync_remote_plan_mode_state_returns_error_without_clobbering_existing_mirror() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("phase", "planning"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plans": [
                    { "plan_id": "plan-9", "goal": "Ship auth" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/plans/plan-9"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "detail": "boom"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();
        state.session_id = Some("sess-1".to_string());
        state.plan_mode = Some(plan::PlanModeState::new("stale goal".to_string()));

        let error = sync_remote_plan_mode_state(&api, "token", &mut state)
            .await
            .expect_err("plan fetch should fail");

        assert!(error.contains("500"), "got: {error}");
        assert_eq!(
            state.plan_mode.as_ref().map(|plan| plan.goal.as_str()),
            Some("stale goal"),
            "failed sync must not overwrite the last known local mirror"
        );
    }

    #[tokio::test]
    async fn exit_remote_plan_mode_approved_clears_local_mirror() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("phase", "planning"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plans": [
                    { "plan_id": "plan-2", "goal": "Ship auth" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/plans/plan-2/exit-plan-mode"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_id": "plan-2",
                "phase": "refining",
                "goal": "Ship auth",
                "version": 3,
                "plan": { "subtasks": [] }
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();
        state.session_id = Some("sess-1".to_string());
        state.plan_mode = Some(plan::PlanModeState::new("Ship auth".to_string()));

        let plan_id = exit_remote_plan_mode(&api, "token", &mut state, true)
            .await
            .unwrap();

        assert_eq!(plan_id.as_deref(), Some("plan-2"));
        assert!(state.plan_mode.is_none());
    }

    #[tokio::test]
    async fn exit_remote_plan_mode_unapproved_propagates_sync_failure_without_clearing_mirror() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("phase", "planning"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plans": [
                    { "plan_id": "plan-2", "goal": "Ship auth" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/plans/plan-2/exit-plan-mode"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_id": "plan-2",
                "phase": "planning"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/plans/plan-2"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "detail": "boom"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();
        state.session_id = Some("sess-1".to_string());
        state.plan_mode = Some(plan::PlanModeState::new("Ship auth".to_string()));

        let error = exit_remote_plan_mode(&api, "token", &mut state, false)
            .await
            .expect_err("follow-up sync should fail");

        assert!(error.contains("500"), "got: {error}");
        assert_eq!(
            state.plan_mode.as_ref().map(|plan| plan.goal.as_str()),
            Some("Ship auth"),
            "failed unapproved exit sync must keep the last known mirror in place"
        );
    }
}
