use crate::cli::cli_config::cli_utils::{
    SessionResumePreflight, local_resumable_last_session_id, preflight_remote_resume_session,
};
use crate::cli::session::session_continuation::{
    SessionContinuation, continuation_activation_names, load_csl_continuation,
    load_session_continuation_for_recovery, sanitize_continuation_messages,
};
use crate::cli::session::session_restore_client::{
    fetch_cloud_session_snapshot_with_client, list_cloud_resumable_sessions,
    restore_session_snapshot_with_client,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct OneShotSessionResumeMetadata {
    pub(crate) completed_turn_count: u32,
    pub(crate) model: Option<String>,
    pub(crate) permission_mode: Option<String>,
    pub(crate) continuation_messages: Vec<serde_json::Value>,
    pub(crate) activated_deferred_tool_names: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct OneShotSessionRouting {
    pub(crate) server_session_id: Option<String>,
    pub(crate) history_source_session_id: Option<String>,
    pub(crate) resume_metadata: OneShotSessionResumeMetadata,
}

impl OneShotSessionRouting {
    fn local_csl_continuation(&self) -> Option<SessionContinuation> {
        let session_id = self.history_source_session_id.as_deref()?;
        match load_csl_continuation(session_id) {
            Ok(Some(local)) => {
                let restored_is_causally_newer =
                    !self.resume_metadata.continuation_messages.is_empty()
                        && self.resume_metadata.completed_turn_count
                            > local.completed_turn_count.unwrap_or_default();
                if restored_is_causally_newer {
                    tracing::info!(
                        %session_id,
                        local_completed_turns = local.completed_turn_count.unwrap_or_default(),
                        restored_completed_turns = self.resume_metadata.completed_turn_count,
                        "using restored continuation because the local CSL projection is behind"
                    );
                    None
                } else {
                    Some(local)
                }
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    %error,
                    "failed to read canonical local continuation; trying restore projection"
                );
                None
            }
        }
    }

    pub(crate) fn continuation(&self) -> Option<SessionContinuation> {
        if let Some(local) = self.local_csl_continuation() {
            return Some(local);
        }
        if !self.resume_metadata.continuation_messages.is_empty() {
            let messages = self.resume_metadata.continuation_messages.clone();
            return Some(SessionContinuation {
                completed_turn_count: Some(self.resume_metadata.completed_turn_count),
                activated_deferred_tool_names: continuation_activation_names(
                    &messages,
                    self.resume_metadata
                        .activated_deferred_tool_names
                        .iter()
                        .cloned(),
                ),
                messages,
            });
        }
        let continuation = self
            .history_source_session_id
            .as_deref()
            .and_then(load_session_continuation_for_recovery)?;
        Some(continuation)
    }

    /// Resolve continuation once, then keep its prompt messages and durable
    /// tool-surface state on the same causal path.
    pub(crate) fn continuation_turn_inputs(&self) -> (Option<Vec<serde_json::Value>>, Vec<String>) {
        match self.continuation() {
            Some(continuation) => (
                Some(continuation.messages),
                continuation.activated_deferred_tool_names,
            ),
            None => (None, Vec::new()),
        }
    }

    pub(crate) fn task_scope_session_id(&self) -> Option<&str> {
        self.server_session_id.as_deref()
    }

    pub(crate) fn restored_model(&self) -> Option<&str> {
        self.resume_metadata.model.as_deref()
    }

    pub(crate) fn restored_permission_mode(&self) -> Option<&str> {
        self.resume_metadata.permission_mode.as_deref()
    }

    /// Return the 1-based turn index for the next Server-owned turn.
    ///
    /// Auxiliary inference runs before the main `/chat/turn` request, so it
    /// cannot rely on that request's stale-turn recovery to repair its causal
    /// scope. A resumed Server session must start from the authoritative
    /// completed-turn count obtained during restore. Local-only continuation
    /// creates a new Server session and therefore always starts at turn 1.
    pub(crate) fn next_server_turn_index(&self) -> u32 {
        if self.server_session_id.is_some() {
            self.resume_metadata.completed_turn_count.saturating_add(1)
        } else {
            1
        }
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
            resume_metadata: OneShotSessionResumeMetadata::default(),
        };
    }

    OneShotSessionRouting {
        history_source_session_id: remote_session_id.clone().or(local_session_id),
        server_session_id: remote_session_id,
        resume_metadata: OneShotSessionResumeMetadata::default(),
    }
}

async fn load_one_shot_resume_metadata(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    session_id: Option<&str>,
    server_session: bool,
) -> OneShotSessionResumeMetadata {
    let Some(session_id) = session_id else {
        return OneShotSessionResumeMetadata::default();
    };

    let mut restored = restore_session_snapshot_with_client(profile, api, session_id).await;
    if server_session
        && restored
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|snapshot| !snapshot.restored_from_cloud)
    {
        // Local CSL/checkpoints remain the highest-fidelity conversation
        // source, but the Server owns the causal turn sequence. Reconcile a
        // local snapshot with the remote turn count so another process or
        // device cannot make auxiliary inference reuse an old ledger key.
        match fetch_cloud_session_snapshot_with_client(profile, api, session_id).await {
            Ok(Some(remote)) => {
                if let Ok(Some(local)) = restored.as_mut() {
                    let remote_is_newer = remote.turn_count > local.turn_count;
                    // Restore sources are independently persisted and can be
                    // briefly out of sync. A completed-turn clock is
                    // monotonic: observing an older remote projection must
                    // never roll a valid local clock backwards.
                    local.turn_count = local.turn_count.max(remote.turn_count);
                    if remote.model.is_some() {
                        local.model = remote.model;
                    }
                    if remote.permission_mode.is_some() {
                        local.permission_mode = remote.permission_mode;
                    }
                    if !remote.conversation_messages.is_empty()
                        && (remote_is_newer || local.conversation_messages.is_empty())
                    {
                        local.conversation_messages = remote.conversation_messages;
                    }
                    local.activated_deferred_tool_names = continuation_activation_names(
                        &[],
                        local
                            .activated_deferred_tool_names
                            .iter()
                            .chain(remote.activated_deferred_tool_names.iter())
                            .cloned(),
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    %error,
                    "failed to reconcile local resume metadata with authoritative server turn"
                );
            }
        }
    }

    match restored {
        Ok(Some(restored)) => {
            let continuation_messages =
                sanitize_continuation_messages(restored.conversation_messages);
            OneShotSessionResumeMetadata {
                completed_turn_count: restored.turn_count,
                model: restored.model,
                permission_mode: restored.permission_mode,
                continuation_messages,
                activated_deferred_tool_names: restored.activated_deferred_tool_names,
            }
        }
        Ok(None) => OneShotSessionResumeMetadata::default(),
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to restore one-shot session metadata; continuing without metadata"
            );
            OneShotSessionResumeMetadata::default()
        }
    }
}

async fn attach_one_shot_resume_metadata(
    mut routing: OneShotSessionRouting,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> OneShotSessionRouting {
    let metadata_session_id = routing
        .history_source_session_id
        .as_deref()
        .or(routing.server_session_id.as_deref());
    routing.resume_metadata = load_one_shot_resume_metadata(
        api,
        profile,
        metadata_session_id,
        routing.server_session_id.is_some(),
    )
    .await;
    routing
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
        let routing = match preflight_remote_resume_session(api, profile, &session_id).await {
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
        };
        return Ok(attach_one_shot_resume_metadata(routing, api, profile).await);
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
    let routing = select_one_shot_session_routing(None, remote_session_id, local_session_id);
    Ok(attach_one_shot_resume_metadata(routing, api, profile).await)
}

#[cfg(test)]
mod tests {
    use super::{
        OneShotSessionResumeMetadata, OneShotSessionRouting, explicit_resume_preflight_error,
        resolve_one_shot_session_routing, select_one_shot_session_routing,
    };
    use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
    use astra_pipeline::step_checkpoint::write_step_checkpoint;
    use astra_pipeline::step_protocol::{
        HeavyCheckpoint, LightCheckpoint, PROTOCOL_VERSION, StepCheckpoint, epoch_ms,
    };
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_local_resumable_session_with_checkpoint(session_id: &str) {
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(session_id, "gpt-5");
        workspace.permission_mode = Some("plan".to_string());
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
            activated_deferred_tool_names: Vec::new(),
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
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        write_step_checkpoint(
            &user_id,
            session_id,
            1,
            &StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
    }

    async fn mock_empty_cloud_resumable_list(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                astra_services::session_restore::ResumableSessionsResponse {
                    sessions: Vec::new(),
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

    async fn mock_existing_session(server: &MockServer, session_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "turn_count": 1
            })))
            .mount(server)
            .await;
    }

    async fn mock_cloud_resume(
        server: &MockServer,
        session_id: &str,
        restored: astra_services::session_restore::RestoredSession,
    ) {
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{session_id}/resume")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(restored))
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
    #[serial_test::serial]
    fn continuation_prefers_local_canonical_evidence_over_lossy_restore_projection() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("routing-canonical-{}", uuid::Uuid::new_v4());
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            1,
            &[
                serde_json::json!({"role": "user", "content": "inspect"}),
                serde_json::json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{}"}
                    }]
                }),
                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": "call-1",
                    "content": "canonical evidence"
                }),
                serde_json::json!({"role": "assistant", "content": "done"}),
            ],
            &astra_turn_core::conversation_log::SessionStateCompact {
                activated_deferred_tool_names: vec!["github".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        let routing = OneShotSessionRouting {
            server_session_id: Some(session_id.clone()),
            history_source_session_id: Some(session_id),
            resume_metadata: OneShotSessionResumeMetadata {
                continuation_messages: vec![
                    serde_json::json!({"role": "user", "content": "inspect"}),
                    serde_json::json!({"role": "assistant", "content": "done"}),
                ],
                ..Default::default()
            },
        };

        let continuation = routing.continuation().expect("continuation");
        assert_eq!(
            continuation.activated_deferred_tool_names,
            vec!["github"],
            "one-shot routing must restore prompt-visible activation from the same canonical projection as its messages"
        );

        assert!(
            continuation
                .messages
                .iter()
                .any(|message| message["role"] == "tool"
                    && message["content"] == "canonical evidence"),
            "a lower-fidelity restore projection must not replace canonical local history"
        );
    }

    #[test]
    #[serial_test::serial]
    fn continuation_prefers_the_projection_with_the_newer_completed_turn() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("routing-causal-{}", uuid::Uuid::new_v4());
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            1,
            &[
                serde_json::json!({"role": "user", "content": "stale local question"}),
                serde_json::json!({"role": "assistant", "content": "stale local answer"}),
            ],
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
        )
        .unwrap();
        let routing = OneShotSessionRouting {
            server_session_id: Some(session_id.clone()),
            history_source_session_id: Some(session_id),
            resume_metadata: OneShotSessionResumeMetadata {
                completed_turn_count: 3,
                continuation_messages: vec![
                    serde_json::json!({"role": "user", "content": "current restored question"}),
                    serde_json::json!({
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "search-1",
                            "function": {"name": "tool_search", "arguments": "{\"query\":\"select:github\"}"}
                        }]
                    }),
                    serde_json::json!({
                        "role": "tool",
                        "tool_call_id": "search-1",
                        "content": serde_json::json!({
                            "mode": "select",
                            "query": "select:github",
                            "requested": ["github"],
                            "matches": [{"name": "github"}],
                            "missing": []
                        }).to_string()
                    }),
                    serde_json::json!({"role": "assistant", "content": "current restored answer"}),
                ],
                ..Default::default()
            },
        };

        let continuation = routing.continuation().expect("continuation");

        assert!(
            continuation
                .messages
                .iter()
                .any(|message| message["content"] == "current restored answer"),
            "history and its completed-turn clock must be selected as one causal projection"
        );
        assert!(
            continuation
                .messages
                .iter()
                .all(|message| message["content"] != "stale local answer"),
            "an older CSL history must not be paired with a newer restored turn clock"
        );
        assert_eq!(
            continuation.activated_deferred_tool_names,
            vec!["github"],
            "a newer restored projection must reconstruct activation from its own durable tool-search evidence"
        );
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
        assert_eq!(routing.restored_model(), Some("gpt-5"));
        assert_eq!(routing.restored_permission_mode(), Some("plan"));

        let continuation = routing
            .continuation()
            .expect("local checkpoint should provide continuation messages")
            .messages;
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
        assert!(routing.continuation().is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_one_shot_session_routing_restores_explicit_local_model_and_mode() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();
        let session_id = uuid::Uuid::new_v4().to_string();
        write_local_resumable_session_with_checkpoint(&session_id);
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            1,
            &[
                serde_json::json!({"role": "user", "content": "previous question"}),
                serde_json::json!({"role": "assistant", "content": "previous answer"}),
            ],
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
        )
        .unwrap();

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
        mock_existing_session(&server, &session_id).await;
        mock_cloud_resume(
            &server,
            &session_id,
            astra_services::session_restore::RestoredSession {
                session_id: session_id.clone(),
                turn_count: 4,
                conversation_messages: vec![
                    serde_json::json!({"role": "user", "content": "question from another device"}),
                    serde_json::json!({"role": "assistant", "content": "latest cloud answer"}),
                ],
                activated_deferred_tool_names: vec!["github".to_string()],
                restored_from_cloud: true,
                ..Default::default()
            },
        )
        .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let routing =
            resolve_one_shot_session_routing(&api, Some("default"), Some(session_id.clone()), true)
                .await
                .expect("explicit local resume should restore metadata");

        assert_eq!(
            routing.server_session_id.as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(routing.restored_model(), Some("gpt-5"));
        assert_eq!(routing.restored_permission_mode(), Some("plan"));
        assert_eq!(
            routing.next_server_turn_index(),
            5,
            "remote causal sequence must override a stale local checkpoint without replacing its richer metadata"
        );
        let continuation = routing
            .continuation()
            .expect("the fresher remote conversation should be resumable")
            .messages;
        assert!(
            continuation
                .iter()
                .any(|message| message["content"] == "latest cloud answer"),
            "a remotely advanced turn count and a stale local conversation must never be combined"
        );
        assert!(
            continuation
                .iter()
                .all(|message| message["content"] != "previous answer"),
            "remote history is authoritative when it is strictly newer"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_one_shot_session_routing_never_regresses_the_completed_turn_clock() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();
        let session_id = uuid::Uuid::new_v4().to_string();
        write_local_resumable_session_with_checkpoint(&session_id);
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&session_id, "gpt-5");
        workspace.turn_count = 3;
        workspace.permission_mode = Some("plan".to_string());
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

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
        mock_existing_session(&server, &session_id).await;
        mock_cloud_resume(
            &server,
            &session_id,
            astra_services::session_restore::RestoredSession {
                session_id: session_id.clone(),
                turn_count: 0,
                restored_from_cloud: true,
                ..Default::default()
            },
        )
        .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let routing =
            resolve_one_shot_session_routing(&api, Some("default"), Some(session_id), true)
                .await
                .expect("resume should tolerate a lagging remote projection");

        assert_eq!(
            routing.next_server_turn_index(),
            4,
            "an eventually consistent restore projection must never roll the local causal clock backwards"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_one_shot_session_routing_uses_cloud_resume_messages_without_local_checkpoint()
    {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();
        let session_id = uuid::Uuid::new_v4().to_string();

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
        mock_existing_session(&server, &session_id).await;
        mock_cloud_resume(
            &server,
            &session_id,
            astra_services::session_restore::RestoredSession {
                session_id: session_id.clone(),
                turn_count: 6,
                model: Some("gpt-5-cloud".to_string()),
                permission_mode: Some("accept_edits".to_string()),
                conversation_messages: vec![
                    serde_json::json!({"role": "user", "content": "cloud question"}),
                    serde_json::json!({"role": "assistant", "content": "cloud answer"}),
                ],
                activated_deferred_tool_names: vec!["github".to_string()],
                restored_from_cloud: true,
                ..Default::default()
            },
        )
        .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let routing =
            resolve_one_shot_session_routing(&api, Some("default"), Some(session_id.clone()), true)
                .await
                .expect("explicit cloud resume should restore messages");

        assert_eq!(routing.restored_model(), Some("gpt-5-cloud"));
        assert_eq!(routing.restored_permission_mode(), Some("accept_edits"));
        assert_eq!(
            routing.next_server_turn_index(),
            7,
            "causal scopes for auxiliary inference must use the restored server turn before the main turn starts"
        );
        let continuation = routing
            .continuation()
            .expect("cloud resume messages should feed one-shot continuation");
        assert_eq!(
            continuation.activated_deferred_tool_names,
            vec!["github"],
            "cloud checkpoint sidecars must survive even when compaction removed tool-search evidence"
        );
        assert_eq!(continuation.messages.len(), 2);
        assert_eq!(continuation.messages[0]["content"], "cloud question");
        assert_eq!(continuation.messages[1]["content"], "cloud answer");
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
