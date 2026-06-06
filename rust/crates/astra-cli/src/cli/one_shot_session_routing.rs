use crate::cli::cli_config::cli_utils::{
    SessionResumePreflight, local_resumable_last_session_id, preflight_remote_resume_session,
};
use crate::cli::session::session_continuation::load_session_messages_for_continuation;
use crate::cli::session::session_restore_client::list_cloud_resumable_sessions;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OneShotSessionRouting {
    pub(crate) server_session_id: Option<String>,
    pub(crate) history_source_session_id: Option<String>,
}

impl OneShotSessionRouting {
    pub(crate) fn continuation_messages(&self) -> Option<Vec<serde_json::Value>> {
        self.history_source_session_id
            .as_deref()
            .and_then(load_session_messages_for_continuation)
    }

    pub(crate) fn task_scope_session_id(&self) -> Option<&str> {
        self.server_session_id.as_deref()
    }
}

fn explicit_resume_preflight_error(session_id: &str, preflight: SessionResumePreflight) -> String {
    match preflight {
        SessionResumePreflight::Missing => format!(
            "Explicit session {session_id} cannot be resumed: the server reports it does not exist."
        ),
        SessionResumePreflight::NoAuth => {
            format!("Explicit session {session_id} cannot be resumed without authentication.")
        }
        SessionResumePreflight::Valid | SessionResumePreflight::Unknown => {
            unreachable!("only definitive explicit resume failures should build a preflight error")
        }
    }
}

pub(crate) fn select_one_shot_session_routing(
    current_session_id: Option<String>,
    remote_session_id: Option<String>,
    local_session_id: Option<String>,
) -> OneShotSessionRouting {
    if let Some(session_id) = current_session_id {
        return OneShotSessionRouting {
            server_session_id: Some(session_id.clone()),
            history_source_session_id: Some(session_id),
        };
    }

    OneShotSessionRouting {
        history_source_session_id: remote_session_id.clone().or(local_session_id),
        server_session_id: remote_session_id,
    }
}

pub(crate) async fn resolve_one_shot_session_routing(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    current_session_id: Option<String>,
    allow_resume: bool,
) -> Result<OneShotSessionRouting, String> {
    if !allow_resume {
        return Ok(OneShotSessionRouting::default());
    }

    if let Some(session_id) = current_session_id {
        return Ok(
            match preflight_remote_resume_session(api, profile, &session_id).await {
                SessionResumePreflight::Missing => {
                    return Err(explicit_resume_preflight_error(
                        &session_id,
                        SessionResumePreflight::Missing,
                    ));
                }
                SessionResumePreflight::NoAuth => {
                    return Err(explicit_resume_preflight_error(
                        &session_id,
                        SessionResumePreflight::NoAuth,
                    ));
                }
                SessionResumePreflight::Valid | SessionResumePreflight::Unknown => {
                    select_one_shot_session_routing(Some(session_id), None, None)
                }
            },
        );
    }

    let local_session_id = local_resumable_last_session_id(profile);
    let remote_session_id = match list_cloud_resumable_sessions(profile, api).await {
        Ok(sessions) => sessions
            .into_iter()
            .find(|session| session.turn_count > 0)
            .map(|session| session.session_id),
        Err(error) => {
            tracing::warn!(
                %error,
                has_local_continuation = local_session_id.is_some(),
                "failed to list cloud resumable sessions; starting without cloud continuation"
            );
            None
        }
    };
    Ok(select_one_shot_session_routing(
        None,
        remote_session_id,
        local_session_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        explicit_resume_preflight_error, resolve_one_shot_session_routing,
        select_one_shot_session_routing,
    };
    use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
    use astra_pipeline::step_checkpoint::write_step_checkpoint;
    use astra_pipeline::step_protocol::{
        HeavyCheckpoint, LightCheckpoint, PROTOCOL_VERSION, StepCheckpoint, epoch_ms,
    };
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_local_resumable_session_with_checkpoint(session_id: &str) {
        let workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(session_id, "gpt-5");
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let heavy = HeavyCheckpoint {
            light: LightCheckpoint {
                protocol_version: PROTOCOL_VERSION,
                cursor: Default::default(),
                step_id: "step-1".to_string(),
                task_id: "task-1".to_string(),
                agent_id: session_id.to_string(),
                progress: 1.0,
                total_tokens: 42,
                created_at: epoch_ms(),
            },
            messages: vec![
                serde_json::json!({"role": "user", "content": "previous question"}),
                serde_json::json!({"role": "assistant", "content": "previous answer"}),
            ],
            budget_remaining_tokens: 0,
            budget_remaining_rounds: 0,
            blocked_tools: Vec::new(),
            recent_tools: Vec::new(),
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            pipeline_state: None,
            compaction_state: None,
            config_version_id: None,
        };
        write_step_checkpoint(session_id, 1, &StepCheckpoint::Heavy(Box::new(heavy))).unwrap();
    }

    async fn mock_empty_cloud_resumable_list(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                astra_services::session_restore::ResumableSessionsResponse {
                    sessions: Vec::new(),
                    limit: 20,
                },
            ))
            .mount(server)
            .await;
    }

    async fn mock_missing_session(server: &MockServer, session_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(404))
            .mount(server)
            .await;
    }

    async fn mock_resumable_list_failure(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(server)
            .await;
    }

    #[test]
    fn select_one_shot_session_routing_uses_active_session_for_server_and_history() {
        let routing = select_one_shot_session_routing(
            Some("active-session".to_string()),
            Some("remote-session".to_string()),
            Some("local-session".to_string()),
        );

        assert_eq!(routing.server_session_id.as_deref(), Some("active-session"));
        assert_eq!(
            routing.history_source_session_id.as_deref(),
            Some("active-session")
        );
        assert_eq!(routing.task_scope_session_id(), Some("active-session"));
    }

    #[test]
    fn select_one_shot_session_routing_keeps_local_history_without_remote_session() {
        let routing =
            select_one_shot_session_routing(None, None, Some("local-session".to_string()));

        assert_eq!(routing.server_session_id, None);
        assert_eq!(
            routing.history_source_session_id.as_deref(),
            Some("local-session")
        );
        assert_eq!(routing.task_scope_session_id(), None);
    }

    #[test]
    fn explicit_resume_preflight_error_describes_missing_and_noauth() {
        let missing = explicit_resume_preflight_error(
            "sess-1",
            crate::cli::cli_config::cli_utils::SessionResumePreflight::Missing,
        );
        assert!(missing.contains("does not exist"));

        let no_auth = explicit_resume_preflight_error(
            "sess-1",
            crate::cli::cli_config::cli_utils::SessionResumePreflight::NoAuth,
        );
        assert!(no_auth.contains("without authentication"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_one_shot_session_routing_keeps_local_continuation_when_cloud_has_no_session() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();
        let session_id = uuid::Uuid::new_v4().to_string();
        write_local_resumable_session_with_checkpoint(&session_id);

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                last_session_id: Some(session_id.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        mock_empty_cloud_resumable_list(&server).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let routing = resolve_one_shot_session_routing(&api, Some("default"), None, true)
            .await
            .expect("routing should resolve");

        assert_eq!(
            routing.server_session_id, None,
            "local-only continuation must not be sent to the server as an active session id"
        );
        assert_eq!(
            routing.history_source_session_id.as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(routing.task_scope_session_id(), None);

        let continuation = routing
            .continuation_messages()
            .expect("local checkpoint should provide continuation messages");
        assert_eq!(continuation.len(), 2);
        assert_eq!(continuation[0]["content"], "previous question");
        assert_eq!(continuation[1]["content"], "previous answer");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_one_shot_session_routing_does_not_fail_when_auto_cloud_listing_fails() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        mock_resumable_list_failure(&server).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let routing = resolve_one_shot_session_routing(&api, Some("default"), None, true)
            .await
            .expect("automatic cloud resume discovery must not block a fresh one-shot turn");

        assert_eq!(routing.server_session_id, None);
        assert_eq!(routing.history_source_session_id, None);
        assert_eq!(routing.task_scope_session_id(), None);
        assert!(routing.continuation_messages().is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_one_shot_session_routing_rejects_missing_explicit_session_without_local_history()
     {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = uuid::Uuid::new_v4().to_string();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                last_session_id: Some(session_id.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        mock_missing_session(&server, &session_id).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let error = resolve_one_shot_session_routing(&api, None, Some(session_id.clone()), true)
            .await
            .expect_err("missing explicit session should fail even without local continuation");

        assert!(
            error.contains("does not exist"),
            "explicit stale session should fail closed before the turn request: {error}"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_one_shot_session_routing_rejects_missing_explicit_session_even_with_local_history()
     {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = uuid::Uuid::new_v4().to_string();
        write_local_resumable_session_with_checkpoint(&session_id);

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        mock_missing_session(&server, &session_id).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let error = resolve_one_shot_session_routing(&api, None, Some(session_id.clone()), true)
            .await
            .expect_err("missing explicit session should fail closed");

        assert!(
            error.contains("cannot be resumed"),
            "unexpected error: {error}"
        );
    }
}
