use astra_services::session_restore::{
    HybridRestoreService, RestoredSession, ResumableSessionsResponse,
};

use crate::cli::cli_config::cli_utils;
use crate::cli::session::session_runtime;

#[derive(serde::Deserialize)]
struct ContinuationResponse {
    session_id: String,
    resume_bundle: astra_turn_types::ResumeBundleV1,
}

async fn request_cloud_resume(
    api: &astra_thin_client::ThinClient,
    bearer: &str,
    path: &str,
    continuation_only: bool,
) -> Result<RestoredSession, astra_thin_client::ThinClientError> {
    if continuation_only {
        let response: ContinuationResponse = api.post_bearer_path_empty_json(bearer, path).await?;
        // Adapt to the existing owner/cursor/root validator; only the bundle
        // leaves this private continuation path, never the unloaded summaries.
        Ok(RestoredSession {
            session_id: response.session_id,
            turn_count: response.resume_bundle.cursor.completed_turn,
            restored_from_cloud: true,
            resume_bundle: Some(response.resume_bundle),
            ..Default::default()
        })
    } else {
        api.post_bearer_path_empty_json(bearer, path).await
    }
}

fn validate_remote_session_id(session_id: &str) -> Result<(), String> {
    cli_utils::validate_cli_session_id(session_id)
}

fn validate_restored_session(
    requested_session_id: &str,
    restored: RestoredSession,
) -> Result<RestoredSession, String> {
    validate_remote_session_id(&restored.session_id)?;
    if restored.session_id != requested_session_id {
        return Err(format!(
            "resume returned mismatched session_id: expected {requested_session_id}, got {}",
            restored.session_id
        ));
    }
    if restored.resume_bundle.is_none() {
        return Err("resume response omitted required versioned ResumeBundle".to_string());
    }
    if let Some(bundle) = restored.resume_bundle.as_ref() {
        let expected_owner = cli_utils::cli_user_id();
        if bundle.schema_version != astra_turn_types::RESUME_BUNDLE_SCHEMA_VERSION {
            return Err(format!(
                "resume bundle schema {} is unsupported",
                bundle.schema_version
            ));
        }
        if bundle.cursor.session_id != requested_session_id {
            return Err(format!(
                "resume bundle returned mismatched session_id: expected {requested_session_id}, got {}",
                bundle.cursor.session_id
            ));
        }
        if bundle.cursor.owner_id != expected_owner {
            return Err(format!(
                "resume bundle returned mismatched owner: expected {expected_owner}, got {}",
                bundle.cursor.owner_id
            ));
        }
        if bundle.cursor.schema_version != astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION {
            return Err(format!(
                "resume cursor schema {} is unsupported",
                bundle.cursor.schema_version
            ));
        }
        if bundle.cursor.projection_schema
            != astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION
            && bundle.cursor.projection_schema
                != astra_turn_types::SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION
        {
            return Err(format!(
                "resume projection schema {} is unsupported",
                bundle.cursor.projection_schema
            ));
        }
        if bundle.cursor.branch_id != astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID {
            return Err(format!(
                "resume bundle branch `{}` is unsupported",
                bundle.cursor.branch_id
            ));
        }
        if bundle.cursor.completed_turn > restored.turn_count {
            return Err(format!(
                "resume bundle turn {} is ahead of authoritative session turn {}",
                bundle.cursor.completed_turn, restored.turn_count
            ));
        }
        if !bundle.validates_root() {
            return Err("resume bundle conversation root validation failed".to_string());
        }
        if !restored.conversation_messages.is_empty() {
            return Err(
                "resume response contains removed standalone conversation_messages".to_string(),
            );
        }
    }
    Ok(restored)
}

fn sanitize_cloud_resumable_sessions(sessions: Vec<RestoredSession>) -> Vec<RestoredSession> {
    sessions
        .into_iter()
        .filter_map(|session| {
            if validate_remote_session_id(&session.session_id).is_ok() {
                Some(session)
            } else {
                tracing::warn!(
                    session_id = %session.session_id,
                    "ignoring cloud resumable session with invalid session id"
                );
                None
            }
        })
        .collect()
}

pub(crate) fn cloud_resume_client() -> Result<Option<astra_thin_client::ThinClient>, String> {
    if let Some(binding) = crate::cli::native_auth::active() {
        return astra_thin_client::ThinClient::new(binding.endpoint(), None)
            .map(|client| Some(client.with_bearer_provider(binding)))
            .map_err(|error| format!("Create cloud API client failed: {error}"));
    }
    let Some(cloud_base) = session_runtime::resolve_cloud_base() else {
        return Ok(None);
    };
    astra_thin_client::ThinClient::new(cloud_base.as_str(), None)
        .map(Some)
        .map_err(|error| format!("Create cloud API client failed: {error}"))
}

pub(crate) fn has_server_auth(profile: Option<&str>) -> bool {
    crate::cli::native_auth::active().is_some()
        || session_runtime::current_access_token(profile).is_some()
}

pub(crate) async fn restore_session_snapshot_with_client(
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    session_id: &str,
) -> Result<Option<RestoredSession>, String> {
    validate_remote_session_id(session_id)?;
    if has_server_auth(profile) {
        return fetch_cloud_session_snapshot_with_client(profile, api, session_id).await;
    }
    HybridRestoreService::local_only()
        .restore_local_session(session_id)
        .await
}

pub(crate) async fn fetch_cloud_session_snapshot_with_client(
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    session_id: &str,
) -> Result<Option<RestoredSession>, String> {
    validate_remote_session_id(session_id)?;
    let native_binding = crate::cli::native_auth::active();
    let token = if let Some(binding) = &native_binding {
        if binding.endpoint().trim_end_matches('/') != api.api_origin().trim_end_matches('/') {
            return Err("Server resume endpoint does not match the selected native account".into());
        }
        // Settle the selected native credential before capturing its owner.
        // Dispatch below uses that explicit binding, not an ambient provider.
        binding
            .access_token()
            .await
            .map_err(|error| format!("Server resume authentication failed: {error}"))?
    } else {
        match session_runtime::current_access_token(profile) {
            Some(token) => token,
            None => return Ok(None),
        }
    };
    let owner = session_runtime::capture_admitted_auth_owner(api, profile, &token).await;
    if native_binding.is_some() && owner.is_none() {
        return Err("Server resume native account changed during authentication".into());
    }
    fetch_cloud_session_snapshot_with_bearer(api, session_id, &token, owner.as_ref(), false)
        .await
        .map(|(restored, _)| restored)
}

/// Read the official continuation through the captured credential binding.
/// Return the successful bearer for subsequent catalog and model requests.
/// An unowned explicit bearer never acquires ambient refresh authority.
async fn fetch_cloud_session_snapshot_with_bearer(
    api: &astra_thin_client::ThinClient,
    session_id: &str,
    token: &str,
    owner: Option<&cli_utils::CliOwnerAuthSnapshot>,
    continuation_only: bool,
) -> Result<(Option<RestoredSession>, String), String> {
    validate_remote_session_id(session_id)?;
    let api = api.clone().without_bearer_provider();
    let mut bearer = match owner {
        Some(owner) => {
            if !owner.is_current() {
                return Err("Server resume account changed before authentication".into());
            }
            session_runtime::owner_access_token(&api, owner, None)
                .await
                .ok_or(
                    "Server resume authentication could not be refreshed for the admitted owner",
                )?
        }
        None => token.to_string(),
    };
    if owner.is_some_and(|owner| !owner.is_current()) {
        return Err("Server resume account changed before request".into());
    }
    let mut path = astra_thin_client::paths::session_resume(session_id);
    if continuation_only {
        path.push_str("?continuation_only=true");
    }
    let response = request_cloud_resume(&api, &bearer, &path, continuation_only).await;
    // Only a typed rejection before execution permits one retry. Refresh the
    // captured binding, never an ambient profile or an environment bearer.
    let response = match (response, owner) {
        (Err(astra_thin_client::ThinClientError::Api { status, .. }), Some(owner))
            if status.as_u16() == 401 =>
        {
            if !owner.is_current() {
                return Err("Server resume account changed before authentication retry".into());
            }
            bearer = session_runtime::owner_access_token(&api, owner, Some(&bearer))
                .await
                .ok_or(
                    "Server resume authentication could not be refreshed for the admitted owner",
                )?;
            if !owner.is_current() {
                return Err("Server resume account changed during authentication retry".into());
            }
            request_cloud_resume(&api, &bearer, &path, continuation_only).await
        }
        (response, _) => response,
    };
    if owner.is_some_and(|owner| !owner.is_current()) {
        return Err("Server resume account changed during request".into());
    }
    match response {
        Ok(restored) => Ok((
            Some(validate_restored_session(session_id, restored)?),
            bearer,
        )),
        Err(astra_thin_client::ThinClientError::Api { status, .. }) if status.as_u16() == 404 => {
            Ok((None, bearer))
        }
        Err(error) => Err(format!("Resume failed: {error}")),
    }
}

/// Refresh only the canonical continuation for an already restored attachment.
pub(crate) async fn fetch_cloud_continuation_with_bearer(
    api: &astra_thin_client::ThinClient,
    session_id: &str,
    token: &str,
    owner: Option<&cli_utils::CliOwnerAuthSnapshot>,
) -> Result<(Option<astra_turn_types::ResumeBundleV1>, String), String> {
    let (session, bearer) =
        fetch_cloud_session_snapshot_with_bearer(api, session_id, token, owner, true).await?;
    let bundle = session.and_then(|session| session.resume_bundle);
    if bundle
        .as_ref()
        .is_some_and(|bundle| bundle.source != astra_turn_types::ResumeSourceV1::CanonicalJournal)
    {
        return Err("Server continuation is no longer canonical; restore the session again before continuing".to_string());
    }
    Ok((bundle, bearer))
}

pub(crate) async fn fetch_cloud_session_snapshot(
    profile: Option<&str>,
    session_id: &str,
) -> Result<Option<RestoredSession>, String> {
    validate_remote_session_id(session_id)?;
    let Some(api) = cloud_resume_client()? else {
        return Ok(None);
    };
    fetch_cloud_session_snapshot_with_client(profile, &api, session_id).await
}

pub(crate) async fn list_cloud_resumable_sessions(
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
) -> Result<Vec<RestoredSession>, String> {
    let Some(token) = session_runtime::current_access_token(profile) else {
        return Ok(Vec::new());
    };
    let response = api
        .get_bearer_path_query_json::<ResumableSessionsResponse>(
            &token,
            astra_thin_client::paths::SESSIONS_RESUMABLE,
            &[("limit", "20".to_string())],
        )
        .await
        .map_err(|error| format!("List resumable sessions failed: {error}"))?;
    Ok(sanitize_cloud_resumable_sessions(response.sessions))
}

#[cfg(test)]
mod tests {
    use super::{
        fetch_cloud_session_snapshot_with_client, list_cloud_resumable_sessions,
        validate_restored_session,
    };
    use crate::cli::cli_config::cli_utils::{self, CredentialsFile, Profile, save_credentials};
    use astra_services::session_restore::{RestoredSession, ResumableSessionsResponse};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn canonical_bundle(owner: &str, session: &str) -> astra_turn_types::ResumeBundleV1 {
        let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: owner.into(),
            session_id: session.into(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.into(),
            completed_turn: 1,
            journal_event_seq: 1,
            conversation_seq: 1,
            canonical_root_hash: astra_turn_types::canonical_conversation_root(&messages),
            projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };
        astra_turn_types::ResumeBundleV1 {
            schema_version: astra_turn_types::RESUME_BUNDLE_SCHEMA_VERSION,
            cursor,
            source: astra_turn_types::ResumeSourceV1::CanonicalJournal,
            conversation_messages: messages,
            materialized_conversation_root_hash: None,
            degraded_reasons: Vec::new(),
            repair_actions: Vec::new(),
            projections: Default::default(),
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn continuation_refresh_uses_canonical_bundle_and_checks_owner() {
        let _identity = cli_utils::install_cli_profile_identity_for_test(
            "continuation-test",
            Some("continuation-owner"),
        )
        .unwrap();
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let bundle = canonical_bundle("continuation-owner", "continuation-session");
        Mock::given(method("POST"))
            .and(path("/sessions/continuation-session/resume"))
            .and(wiremock::matchers::query_param("continuation_only", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "continuation-session",
                "resume_bundle": bundle,
            })))
            .expect(2)
            .mount(&server)
            .await;
        let (restored, bearer) = super::fetch_cloud_continuation_with_bearer(
            &api,
            "continuation-session",
            "token",
            None,
        )
        .await
        .unwrap();
        assert_eq!(bearer, "token");
        assert_eq!(restored.unwrap().cursor.owner_id, "continuation-owner");

        let _other = cli_utils::install_cli_profile_identity_for_test(
            "continuation-other",
            Some("other-owner"),
        )
        .unwrap();
        let error = super::fetch_cloud_continuation_with_bearer(
            &api,
            "continuation-session",
            "token",
            None,
        )
        .await
        .unwrap_err();
        assert!(error.contains("mismatched owner"), "{error}");
        server.verify().await;

        drop(_other);
        let fallback_server = MockServer::start().await;
        let fallback_api =
            astra_thin_client::ThinClient::new(&fallback_server.uri(), None).unwrap();
        let mut fallback = canonical_bundle("continuation-owner", "continuation-session");
        fallback.source = astra_turn_types::ResumeSourceV1::Checkpoint;
        Mock::given(method("POST"))
            .and(path("/sessions/continuation-session/resume"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "continuation-session", "resume_bundle": fallback,
            })))
            .mount(&fallback_server)
            .await;
        let error = super::fetch_cloud_continuation_with_bearer(
            &fallback_api,
            "continuation-session",
            "token",
            None,
        )
        .await
        .unwrap_err();
        assert!(error.contains("no longer canonical"), "{error}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_resume_uses_selected_binding_and_rejects_another_origin() {
        let _credentials = crate::tests::isolate_credentials();
        let root = tempfile::tempdir().unwrap();
        let store =
            astra_credentials::native::NativeStore::with_directory(root.path().join("native"));
        let server = MockServer::start().await;
        let other = MockServer::start().await;
        let mut session = crate::cli::native_auth::test_session();
        session.environment.astra_url = server.uri();
        let (session, _) = store.publish(session).unwrap();
        let binding = crate::cli::native_auth::binding_for_test(store, session.clone());
        let _active = crate::cli::native_auth::install_active_for_test(binding.clone());
        let _identity = cli_utils::install_cli_profile_identity_for_test(
            &binding.profile_name(),
            Some(&session.astra_user_id),
        )
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/sessions/native-resume/resume"))
            .and(wiremock::matchers::header(
                "authorization",
                format!("Bearer {}", session.access_token),
            ))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None)
            .unwrap()
            .with_bearer_provider(binding);
        assert!(
            super::fetch_cloud_session_snapshot_with_client(None, &api, "native-resume")
                .await
                .unwrap()
                .is_none()
        );
        let other_api = astra_thin_client::ThinClient::new(&other.uri(), None).unwrap();
        assert!(
            super::fetch_cloud_session_snapshot_with_client(None, &other_api, "native-resume")
                .await
                .unwrap_err()
                .contains("endpoint")
        );
        assert!(other.received_requests().await.unwrap().is_empty());
        server.verify().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resume_auth_retry_is_once_owner_bound_and_only_for_401() {
        use wiremock::matchers::header;
        let _credentials = crate::tests::isolate_credentials();
        let _environment = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        for (owned, initial_status, refresh_status, retry_status, continuation_only) in [
            (true, 401, 200, 401, true),
            (true, 401, 200, 403, false),
            (true, 401, 503, 200, false),
            (true, 403, 200, 200, false),
            (false, 401, 200, 200, false),
        ] {
            let mut credentials = CredentialsFile::default();
            credentials.profiles.insert(
                "default".into(),
                Profile {
                    account_id: Some("resume-auth-owner".into()),
                    access_token: Some("owner-old-access".into()),
                    refresh_token: Some("owner-refresh".into()),
                    ..Default::default()
                },
            );
            save_credentials(&credentials).unwrap();
            let _identity =
                crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
                    "default",
                    Some("resume-auth-owner"),
                )
                .unwrap();
            let server = MockServer::start().await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let token = if owned {
                "owner-old-access"
            } else {
                "unowned-explicit-access"
            };
            let owner = crate::cli::session::session_runtime::capture_admitted_auth_owner(
                &api, None, token,
            )
            .await;
            assert_eq!(owner.is_some(), owned);
            let refresh_expected = owned && initial_status == 401;
            let retry_expected = refresh_expected && refresh_status == 200;
            Mock::given(method("POST"))
                .and(path("/sessions/auth-retry/resume"))
                .and(header("authorization", format!("Bearer {token}")))
                .respond_with(ResponseTemplate::new(initial_status))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST")).and(path("/auth/refresh"))
                .and(wiremock::matchers::body_json(serde_json::json!({"refresh_token":"owner-refresh"})))
                .respond_with(ResponseTemplate::new(refresh_status).set_body_json(serde_json::json!({
                    "user_id":"resume-auth-owner", "access_token":"owner-new-access", "refresh_token":"owner-new-refresh",
                }))).expect(u64::from(refresh_expected)).mount(&server).await;
            Mock::given(method("POST"))
                .and(path("/sessions/auth-retry/resume"))
                .and(header("authorization", "Bearer owner-new-access"))
                .respond_with(ResponseTemplate::new(retry_status))
                .expect(u64::from(retry_expected))
                .mount(&server)
                .await;
            assert!(
                super::fetch_cloud_session_snapshot_with_bearer(
                    &api,
                    "auth-retry",
                    token,
                    owner.as_ref(),
                    continuation_only,
                )
                .await
                .is_err()
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(
                requests.len(),
                1 + usize::from(refresh_expected) + usize::from(retry_expected)
            );
            server.verify().await;
        }
    }

    #[test]
    #[serial_test::serial]
    fn restored_bundle_must_belong_to_the_bound_account() {
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "owner-validation",
            Some("account-a"),
        )
        .unwrap();
        let bundle = canonical_bundle("account-b", "shared-session");

        let error = validate_restored_session(
            "shared-session",
            RestoredSession {
                session_id: "shared-session".into(),
                turn_count: 1,
                resume_bundle: Some(bundle),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("mismatched owner"), "{error}");
    }

    #[test]
    fn remote_restore_requires_a_bundle_even_for_an_empty_session() {
        let error = validate_restored_session(
            "empty-session",
            RestoredSession {
                session_id: "empty-session".into(),
                turn_count: 0,
                resume_bundle: None,
                conversation_messages: Vec::new(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("required versioned ResumeBundle"), "{error}");
    }

    async fn mock_cloud_resumable_list(server: &MockServer, sessions: &[RestoredSession]) {
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ResumableSessionsResponse {
                    sessions: sessions.to_vec(),
                }),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fetch_cloud_session_snapshot_with_client_rejects_invalid_session_id() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let error = fetch_cloud_session_snapshot_with_client(None, &api, "../escape")
            .await
            .expect_err("invalid session id must fail before any request");

        assert!(error.contains("invalid session_id"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn list_cloud_resumable_sessions_filters_invalid_session_ids() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        mock_cloud_resumable_list(
            &server,
            &[
                RestoredSession {
                    session_id: "valid-session-1".to_string(),
                    turn_count: 3,
                    ..Default::default()
                },
                RestoredSession {
                    session_id: "../escape".to_string(),
                    turn_count: 5,
                    ..Default::default()
                },
            ],
        )
        .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let sessions = list_cloud_resumable_sessions(None, &api)
            .await
            .expect("list should succeed");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "valid-session-1");
    }
}
