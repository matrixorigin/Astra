use super::*;
use astra_runtime::plan;

fn pending_plan_state() -> plan::PlanModeState {
    plan::PlanModeState::new(String::new())
}

async fn resolve_plan_token(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: Option<&str>,
) -> Option<String> {
    if let Some(token) = token {
        return Some(token.to_string());
    }
    crate::plan_lifecycle::fresh_token_for_plan(api, profile).await
}

pub(super) async fn handle_plan_command(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
    token: Option<&str>,
) -> Result<(), String> {
    let plan_request = arg.trim();

    if plan_request.is_empty() && crate::plan_lifecycle::looks_like_pending_local_plan_entry(state)
    {
        state.cloud_plan_mirror = None;
        state.plan_mode_sync_error = None;
        eprintln!("  {} Exited plan mode.", theme::icon_ok());
        return Ok(());
    }

    if plan_request.is_empty() && state.cloud_plan_mirror.is_some() {
        let Some(token) = resolve_plan_token(api, profile, token).await else {
            eprintln!("{}", "  Not logged in. Use /login.".yellow());
            return Ok(());
        };
        let plan_id =
            crate::plan_lifecycle::exit_remote_plan_mode(api, &token, state, true).await?;
        if let Some(plan_id) = plan_id {
            eprintln!(
                "  {} Exited plan mode. Approved plan: {}",
                theme::icon_ok(),
                plan_id
            );
        } else {
            eprintln!("  {} Exited plan mode.", theme::icon_ok());
        }
        return Ok(());
    }

    let Some(token) = resolve_plan_token(api, profile, token).await else {
        eprintln!("{}", "  Not logged in. Use /login.".yellow());
        return Ok(());
    };

    if plan_request.is_empty() {
        state.cloud_plan_mirror = Some(pending_plan_state());
        state.plan_mode_sync_error = None;
        eprintln!();
        eprintln!(
            "  {} Plan mode active. Describe your goal.",
            theme::icon_ok()
        );
        return Ok(());
    }

    crate::plan_lifecycle::enter_remote_plan_mode(api, profile, &token, state, plan_request)
        .await?;
    eprintln!(
        "  {} Plan mode active. Goal: {}",
        theme::icon_ok(),
        plan_request
    );
    crate::chat_turn::handle_chat_input(
        plan_request.to_string(),
        Some(&token),
        state,
        crate::chat_turn::TurnContext { api, profile },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn bare_plan_arms_pending_local_entry() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();

        handle_plan_command("", &api, None, &mut state, Some("token"))
            .await
            .unwrap();

        assert!(crate::plan_lifecycle::looks_like_pending_local_plan_entry(
            &state
        ));
    }

    #[tokio::test]
    async fn bare_plan_exits_pending_local_entry_without_remote_call() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        state.cloud_plan_mirror = Some(plan::PlanModeState::new(String::new()));

        handle_plan_command("", &api, None, &mut state, None)
            .await
            .unwrap();

        assert!(state.cloud_plan_mirror.is_none());
    }

    #[tokio::test]
    async fn bare_plan_from_active_remote_mode_exits_via_remote_lifecycle() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("phase", "planning"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plans": [
                    { "plan_id": "plan-1", "goal": "Ship auth" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/plans/plan-1/exit-plan-mode"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_id": "plan-1",
                "phase": "refining",
                "goal": "Ship auth",
                "version": 7,
                "plan": { "subtasks": [] }
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = SessionState::default();
        state.session_id = Some("sess-1".to_string());
        state.cloud_plan_mirror = Some(plan::PlanModeState::new("Ship auth".to_string()));

        handle_plan_command("", &api, None, &mut state, Some("token"))
            .await
            .unwrap();

        assert!(state.cloud_plan_mirror.is_none());
    }

    #[test]
    fn slash_memory_no_longer_owns_plan_surface() {
        let src = include_str!("slash_memory.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        assert!(
            !prod.contains("\"/plan\" =>"),
            "legacy /plan fallback should be removed from slash_memory.rs"
        );
    }
}
