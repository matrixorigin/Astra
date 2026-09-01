use crate::cli::cli_config::cli_utils::{
    SessionResumePreflight, local_resumable_last_session_id, preflight_remote_resume_session,
};
use crate::cli::session::session_continuation::{
    SessionContinuation, continuation_from_resume_bundle, load_session_continuation_for_recovery,
    portable_resume_descriptor, sanitize_continuation_messages,
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
    pub(crate) resume_bundle: Option<astra_turn_types::ResumeBundleV1>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct OneShotSessionRouting {
    pub(crate) server_session_id: Option<String>,
    pub(crate) history_source_session_id: Option<String>,
    pub(crate) resume_metadata: OneShotSessionResumeMetadata,
}

impl OneShotSessionRouting {
    fn take_continuation(&mut self) -> Result<Option<SessionContinuation>, String> {
        let remote_bundle = self.resume_metadata.resume_bundle.take();
        if self.server_session_id.is_some() && remote_bundle.is_none() {
            return Err(
                "selected Server session omitted required versioned ResumeBundle".to_string(),
            );
        }
        if let (Some(expected_session_id), Some(bundle)) =
            (self.server_session_id.as_deref(), remote_bundle.as_ref())
            && bundle.cursor.session_id != expected_session_id
        {
            return Err(format!(
                "remote ResumeBundle belongs to session `{}`, expected `{expected_session_id}`",
                bundle.cursor.session_id
            ));
        }

        // A canonical bundle returned for an attached Server session is the
        // authority, while the CLI journal is only a local replica. Their
        // journal sequence numbers are allocated by independent stores and
        // therefore cannot participate in one cursor ordering. Selecting the
        // Server bundle directly also avoids replaying a potentially long
        // local journal merely to compare incomparable clocks.
        if self.server_session_id.is_some()
            && remote_bundle.as_ref().is_some_and(|bundle| {
                bundle.source == astra_turn_types::ResumeSourceV1::CanonicalJournal
            })
        {
            let remote = remote_bundle.expect("canonical remote bundle was checked above");
            if !remote.validates_root() {
                tracing::warn!(
                    "remote one-shot resume bundle does not materialize its declared root"
                );
                return Err(
                    "remote one-shot resume bundle failed canonical root validation".to_string(),
                );
            }
            return continuation_from_resume_bundle(remote)
                .map(Some)
                .ok_or_else(|| "remote ResumeBundle failed causal validation".to_string());
        }

        let local = self
            .history_source_session_id
            .as_deref()
            .and_then(load_session_continuation_for_recovery);

        let mut candidates = Vec::with_capacity(2);
        if let Some(local) = local.as_ref() {
            candidates.push(portable_resume_descriptor(local.resume.clone()));
        }
        if let Some(remote) = remote_bundle.as_ref() {
            if !remote.validates_root() {
                tracing::warn!(
                    "remote one-shot resume bundle does not materialize its declared root"
                );
                return Err(
                    "remote one-shot resume bundle failed canonical root validation".to_string(),
                );
            }
            candidates.push(portable_resume_descriptor(remote.descriptor()));
        }
        if candidates.is_empty() {
            return Ok(None);
        }
        let selected = astra_turn_types::select_resume_candidate_index(None, &candidates).map_err(
            |error| {
                tracing::warn!(
                    %error,
                    "one-shot resume candidates do not share one causal generation"
                );
                format!("one-shot resume candidates are causally inconsistent: {error}")
            },
        )?;
        if selected == 0 && local.is_some() {
            return Ok(local);
        }
        let had_remote_bundle = remote_bundle.is_some();
        let continuation = remote_bundle.and_then(continuation_from_resume_bundle);
        if had_remote_bundle && continuation.is_none() {
            return Err("remote ResumeBundle failed causal validation".to_string());
        }
        Ok(continuation)
    }

    #[cfg(test)]
    pub(crate) fn continuation(&self) -> Result<Option<SessionContinuation>, String> {
        self.clone().take_continuation()
    }

    /// Resolve continuation once, then keep its prompt messages and durable
    /// tool-surface state on the same causal path.
    pub(crate) fn continuation_turn_inputs(
        &mut self,
    ) -> Result<(Option<Vec<serde_json::Value>>, Vec<String>), String> {
        Ok(match self.take_continuation()? {
            Some(continuation) => (
                Some(continuation.messages),
                continuation.activated_deferred_tool_names,
            ),
            None => (None, Vec::new()),
        })
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
    /// Auxiliary inference runs before the main `/chat/stream` request, so it
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
) -> Result<OneShotSessionResumeMetadata, String> {
    let Some(session_id) = session_id else {
        return Ok(OneShotSessionResumeMetadata::default());
    };

    let mut restored = match restore_session_snapshot_with_client(profile, api, session_id).await {
        Ok(restored) => restored,
        Err(error) if server_session => {
            return Err(format!(
                "selected Server session {session_id} could not be restored: {error}"
            ));
        }
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to restore automatic local continuation; starting a new session"
            );
            return Ok(OneShotSessionResumeMetadata::default());
        }
    };
    if server_session
        && restored
            .as_ref()
            .is_some_and(|snapshot| !snapshot.restored_from_cloud)
    {
        // The Server owns both the canonical conversation and turn sequence
        // for an attached session. Local state can locate that session, but
        // cannot replace missing Server resume authority.
        match fetch_cloud_session_snapshot_with_client(profile, api, session_id).await {
            Ok(Some(remote)) => {
                if let Some(local) = restored.as_mut() {
                    // The Server turn clock is authoritative for a networked
                    // session. Conversation selection remains cursor/root
                    // based in `continuation`; do not splice remote messages
                    // into a local snapshot based on turn counts.
                    local.turn_count = remote.turn_count;
                    // Conversation-sensitive provider state must come from
                    // the same server-selected generation. An absent value is
                    // meaningful; retaining a richer local value would splice
                    // stale provider state into the remote conversation.
                    local.model = remote.model.clone();
                    local.permission_mode = remote.permission_mode.clone();
                    local.conversation_messages = remote.conversation_messages;
                    local.activated_deferred_tool_names = remote.activated_deferred_tool_names;
                    local.resume_bundle = remote.resume_bundle;
                }
            }
            Ok(None) => {
                return Err(format!(
                    "selected Server session {session_id} has no authoritative restore bundle"
                ));
            }
            Err(error) => {
                return Err(format!(
                    "selected Server session {session_id} could not load its authoritative restore bundle: {error}"
                ));
            }
        }
    }

    match restored {
        Some(restored) => {
            let continuation_messages = if restored.resume_bundle.is_some() {
                Vec::new()
            } else {
                sanitize_continuation_messages(restored.conversation_messages)
            };
            if server_session && restored.resume_bundle.is_none() {
                return Err(format!(
                    "selected Server session {session_id} omitted required versioned ResumeBundle"
                ));
            }
            Ok(OneShotSessionResumeMetadata {
                // Server turn admission and conversation hydration are
                // separate deterministic planes. A checkpoint may lag the
                // Server head; it must not roll back the next turn index.
                completed_turn_count: restored.turn_count,
                model: restored.model,
                permission_mode: restored.permission_mode,
                continuation_messages,
                resume_bundle: restored.resume_bundle,
            })
        }
        None if server_session => Err(format!(
            "selected Server session {session_id} has no restorable canonical state"
        )),
        None => Ok(OneShotSessionResumeMetadata::default()),
    }
}

async fn attach_one_shot_resume_metadata(
    mut routing: OneShotSessionRouting,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Result<OneShotSessionRouting, String> {
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
    .await?;
    Ok(routing)
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
        return attach_one_shot_resume_metadata(routing, api, profile).await;
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
    attach_one_shot_resume_metadata(routing, api, profile).await
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

        let messages = vec![
            serde_json::json!({"role": "user", "content": "previous question"}),
            serde_json::json!({"role": "assistant", "content": "previous answer"}),
        ];
        let cursor = astra_turn_core::active_conversation::ActiveConversation::empty(
            astra_services::local_owner_scope().id(),
            session_id,
        )
        .unwrap()
        .prepare_commit(1, None, messages.clone())
        .unwrap()
        .next
        .cursor()
        .clone();
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
            conversation_cursor: Some(cursor),
            messages,
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
            workspace_observation_quarantine: None,
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

    fn typed_resume_bundle(
        session_id: &str,
        sequence: u64,
        messages: Vec<serde_json::Value>,
        activated_deferred_tool_names: Vec<String>,
    ) -> astra_turn_types::ResumeBundleV1 {
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: crate::cli::cli_config::cli_utils::cli_user_id(),
            session_id: session_id.to_string(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.to_string(),
            completed_turn: sequence as u32,
            journal_event_seq: sequence,
            conversation_seq: sequence,
            canonical_root_hash: astra_turn_types::canonical_conversation_root(&messages),
            projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };
        let activation = astra_turn_types::CausalProjectionEnvelopeV1::at_cursor(
            cursor.clone(),
            astra_turn_types::ResumeActivationProjectionV1 {
                deferred_tool_names: activated_deferred_tool_names,
            },
        );
        astra_turn_types::select_resume_bundle(
            None,
            [astra_turn_types::ResumeCandidateV1 {
                source: astra_turn_types::ResumeSourceV1::Checkpoint,
                cursor,
                conversation_messages: messages,
                materialized_conversation_root_hash: None,
                degraded_reasons: Vec::new(),
                repair_actions: Vec::new(),
                projections: astra_turn_types::ResumeProjectionSetV1 {
                    activation: Some(activation),
                    ..Default::default()
                },
            }],
        )
        .unwrap()
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

    async fn mock_missing_resume(server: &MockServer, session_id: &str) {
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{session_id}/resume")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(404))
            .mount(server)
            .await;
    }

    async fn mock_cloud_resumable_session(server: &MockServer, session_id: &str) {
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                astra_services::session_restore::ResumableSessionsResponse {
                    sessions: vec![astra_services::session_restore::RestoredSession {
                        session_id: session_id.to_string(),
                        turn_count: 1,
                        ..Default::default()
                    }],
                },
            ))
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
    fn continuation_rejects_unversioned_remote_projection_before_local_selection() {
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

        let error = routing
            .continuation()
            .expect_err("unversioned remote history must fail closed");
        assert!(error.contains("required versioned ResumeBundle"), "{error}");
    }

    #[test]
    #[serial_test::serial]
    fn continuation_prefers_the_newer_typed_causal_projection() {
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
        let remote_messages = vec![
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
        ];
        let routing = OneShotSessionRouting {
            server_session_id: Some(session_id.clone()),
            history_source_session_id: Some(session_id.clone()),
            resume_metadata: OneShotSessionResumeMetadata {
                completed_turn_count: 3,
                resume_bundle: Some(typed_resume_bundle(
                    &session_id,
                    3,
                    remote_messages,
                    Vec::new(),
                )),
                ..Default::default()
            },
        };

        let continuation = routing
            .continuation()
            .expect("causal selection")
            .expect("continuation");

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
    #[serial_test::serial]
    fn attached_session_uses_server_canonical_authority_across_journal_clock_domains() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("routing-authority-{}", uuid::Uuid::new_v4());
        let owner_id = astra_services::local_owner_scope().id().to_string();
        let server_messages = vec![
            serde_json::json!({"role": "user", "content": "server question"}),
            serde_json::json!({"role": "assistant", "content": "server answer"}),
        ];
        let active =
            astra_turn_core::active_conversation::ActiveConversation::empty(&owner_id, &session_id)
                .unwrap();
        let first = active
            .prepare_commit(1, None, server_messages.clone())
            .unwrap();
        let mut local_messages = server_messages.clone();
        local_messages.extend([
            serde_json::json!({"role": "user", "content": "unacknowledged local question"}),
            serde_json::json!({"role": "assistant", "content": "unacknowledged local answer"}),
        ]);
        let second = first.next.prepare_commit(2, None, local_messages).unwrap();
        let writer = astra_services::session_journal::JournalWriter::new(&session_id).unwrap();
        for (turn, commit) in [(1, first.commit), (2, second.commit)] {
            writer
                .append(
                    &astra_services::session_journal::JournalEvent::turn(
                        Some(&session_id),
                        turn,
                        Some("test-model"),
                        "display user",
                        "display assistant",
                        0,
                        0,
                        0,
                        0,
                    )
                    .with_conversation_commit(commit),
                )
                .unwrap();
        }

        let mut server_bundle = typed_resume_bundle(&session_id, 1, server_messages, Vec::new());
        server_bundle.source = astra_turn_types::ResumeSourceV1::CanonicalJournal;
        // Server and CLI journals allocate event sequences independently. A
        // larger Server event sequence combined with a smaller conversation
        // sequence is not a fork and must never be compared as one clock.
        server_bundle.cursor.journal_event_seq = 100;
        let routing = OneShotSessionRouting {
            server_session_id: Some(session_id.clone()),
            history_source_session_id: Some(session_id),
            resume_metadata: OneShotSessionResumeMetadata {
                completed_turn_count: 1,
                resume_bundle: Some(server_bundle),
                ..Default::default()
            },
        };

        let continuation = routing
            .continuation()
            .expect("Server authority must resolve independent journal clocks")
            .expect("canonical Server continuation");

        assert!(
            continuation
                .messages
                .iter()
                .any(|message| message["content"] == "server answer")
        );
        assert!(
            continuation
                .messages
                .iter()
                .all(|message| message["content"] != "unacknowledged local answer")
        );
        assert_eq!(continuation.resume.cursor.journal_event_seq, 100);
    }

    #[test]
    fn one_shot_consumes_remote_history_instead_of_retaining_an_extra_copy() {
        let session_id = "long-one-shot";
        let messages = (0..2_048)
            .map(|index| serde_json::json!({"role": "user", "content": format!("message-{index}")}))
            .collect::<Vec<_>>();
        let mut routing = OneShotSessionRouting {
            server_session_id: Some(session_id.into()),
            history_source_session_id: None,
            resume_metadata: OneShotSessionResumeMetadata {
                completed_turn_count: 1_024,
                resume_bundle: Some(typed_resume_bundle(session_id, 1_024, messages, Vec::new())),
                ..Default::default()
            },
        };

        let continuation = routing.take_continuation().unwrap().unwrap();
        assert_eq!(continuation.messages.len(), 2_048);
        assert!(
            routing.resume_metadata.resume_bundle.is_none(),
            "routing must release its large wire payload once continuation owns it"
        );
        assert!(routing.resume_metadata.continuation_messages.is_empty());
    }

    #[test]
    fn one_shot_surfaces_typed_identity_conflicts_instead_of_starting_with_empty_history() {
        let session_id = "identity-conflict";
        let mut bundle = typed_resume_bundle(
            session_id,
            1,
            vec![serde_json::json!({"role": "user", "content": "private"})],
            Vec::new(),
        );
        bundle.cursor.owner_id = "another-account".into();
        let mut routing = OneShotSessionRouting {
            server_session_id: Some(session_id.into()),
            history_source_session_id: None,
            resume_metadata: OneShotSessionResumeMetadata {
                resume_bundle: Some(bundle),
                ..Default::default()
            },
        };

        let error = routing.take_continuation().unwrap_err();
        assert!(error.contains("failed causal validation"), "{error}");
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
            .expect("causal selection")
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
        assert!(routing.continuation().unwrap().is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn networked_resume_does_not_splice_stale_local_provider_state() {
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
        let remote_messages = vec![
            serde_json::json!({"role": "user", "content": "question from another device"}),
            serde_json::json!({"role": "assistant", "content": "latest cloud answer"}),
        ];
        mock_cloud_resume(
            &server,
            &session_id,
            astra_services::session_restore::RestoredSession {
                session_id: session_id.clone(),
                turn_count: 4,
                resume_bundle: Some(typed_resume_bundle(
                    &session_id,
                    4,
                    remote_messages,
                    vec!["github".to_string()],
                )),
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
        assert_eq!(routing.restored_model(), None);
        assert_eq!(routing.restored_permission_mode(), None);
        assert_eq!(
            routing.next_server_turn_index(),
            5,
            "the selected remote generation owns the next Server turn"
        );
        let continuation = routing
            .continuation()
            .expect("causal selection")
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
    async fn networked_resume_rejects_an_authoritative_response_without_a_bundle() {
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

        let error = resolve_one_shot_session_routing(&api, Some("default"), Some(session_id), true)
            .await
            .expect_err("a selected Server session must not resume without a typed bundle");

        assert!(
            error.contains("required versioned ResumeBundle"),
            "unexpected error: {error}"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn explicit_server_session_propagates_missing_restore_state() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
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
        mock_missing_resume(&server, &session_id).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let error =
            resolve_one_shot_session_routing(&api, Some("default"), Some(session_id.clone()), true)
                .await
                .expect_err("an explicit session with no restore state must fail closed");

        assert!(error.contains("no restorable canonical state"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn automatically_selected_server_session_propagates_missing_restore_state() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
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
        mock_cloud_resumable_session(&server, &session_id).await;
        mock_missing_resume(&server, &session_id).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let error = resolve_one_shot_session_routing(&api, Some("default"), None, true)
            .await
            .expect_err("a selected Server session with no restore state must fail closed");

        assert!(error.contains("no restorable canonical state"), "{error}");
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
        let cloud_messages = vec![
            serde_json::json!({"role": "user", "content": "cloud question"}),
            serde_json::json!({"role": "assistant", "content": "cloud answer"}),
        ];
        mock_cloud_resume(
            &server,
            &session_id,
            astra_services::session_restore::RestoredSession {
                session_id: session_id.clone(),
                turn_count: 6,
                model: Some("gpt-5-cloud".to_string()),
                permission_mode: Some("accept_edits".to_string()),
                resume_bundle: Some(typed_resume_bundle(
                    &session_id,
                    5,
                    cloud_messages,
                    vec!["github".to_string()],
                )),
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
            "a lagging checkpoint cursor must not roll back the authoritative Server turn"
        );
        let continuation = routing
            .continuation()
            .expect("causal selection")
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
