use astra_services::session_restore::{
    HybridRestoreService, RestoredSession, ResumableSessionsResponse,
};

use crate::cli::cli_config::cli_utils;
use crate::cli::session::session_runtime;

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
        if bundle.cursor.schema_version != 0
            && bundle.cursor.schema_version != astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION
        {
            return Err(format!(
                "resume cursor schema {} is unsupported",
                bundle.cursor.schema_version
            ));
        }
        if bundle.cursor.schema_version != 0
            && bundle.cursor.projection_schema
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
        if !restored.conversation_messages.is_empty()
            && restored.conversation_messages != bundle.conversation_messages
        {
            return Err(
                "resume response contains divergent legacy messages and resume bundle".to_string(),
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
    let Some(cloud_base) = session_runtime::resolve_cloud_base() else {
        return Ok(None);
    };
    astra_thin_client::ThinClient::new(cloud_base.as_str(), None)
        .map(Some)
        .map_err(|error| format!("Create cloud API client failed: {error}"))
}

pub(crate) async fn restore_session_snapshot_with_client(
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    session_id: &str,
) -> Result<Option<RestoredSession>, String> {
    validate_remote_session_id(session_id)?;
    let local = HybridRestoreService::local_only()
        .restore_local_session(session_id)
        .await?;
    if local.is_some() {
        return Ok(local);
    }

    let Some(token) = session_runtime::current_access_token(profile) else {
        return Ok(None);
    };
    let path = astra_thin_client::paths::session_resume(session_id);
    match api
        .post_bearer_path_empty_json::<RestoredSession>(&token, &path)
        .await
    {
        Ok(restored) => Ok(Some(validate_restored_session(session_id, restored)?)),
        Err(astra_thin_client::ThinClientError::Api { status, .. }) if status.as_u16() == 404 => {
            Ok(None)
        }
        Err(error) => Err(format!("Resume failed: {error}")),
    }
}

pub(crate) async fn fetch_cloud_session_snapshot_with_client(
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    session_id: &str,
) -> Result<Option<RestoredSession>, String> {
    validate_remote_session_id(session_id)?;
    let Some(token) = session_runtime::current_access_token(profile) else {
        return Ok(None);
    };
    let path = astra_thin_client::paths::session_resume(session_id);
    match api
        .post_bearer_path_empty_json::<RestoredSession>(&token, &path)
        .await
    {
        Ok(restored) => Ok(Some(validate_restored_session(session_id, restored)?)),
        Err(astra_thin_client::ThinClientError::Api { status, .. }) if status.as_u16() == 404 => {
            Ok(None)
        }
        Err(error) => Err(format!("Resume failed: {error}")),
    }
}

pub(crate) async fn restore_session_snapshot(
    profile: Option<&str>,
    session_id: &str,
) -> Result<Option<RestoredSession>, String> {
    validate_remote_session_id(session_id)?;
    let local = HybridRestoreService::local_only()
        .restore_local_session(session_id)
        .await?;
    if local.is_some() {
        return Ok(local);
    }

    let Some(api) = cloud_resume_client()? else {
        return Ok(None);
    };
    let Some(token) = session_runtime::current_access_token(profile) else {
        return Ok(None);
    };
    let path = astra_thin_client::paths::session_resume(session_id);
    match api
        .post_bearer_path_empty_json::<RestoredSession>(&token, &path)
        .await
    {
        Ok(restored) => Ok(Some(validate_restored_session(session_id, restored)?)),
        Err(astra_thin_client::ThinClientError::Api { status, .. }) if status.as_u16() == 404 => {
            Ok(None)
        }
        Err(error) => Err(format!("Resume failed: {error}")),
    }
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
    use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
    use astra_services::session_restore::{RestoredSession, ResumableSessionsResponse};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    #[serial_test::serial]
    fn restored_bundle_must_belong_to_the_bound_account() {
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "owner-validation",
            Some("account-a"),
        )
        .unwrap();
        let messages = vec![serde_json::json!({"role": "user", "content": "private"})];
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: "account-b".into(),
            session_id: "shared-session".into(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.into(),
            completed_turn: 1,
            journal_event_seq: 1,
            conversation_seq: 1,
            canonical_root_hash: astra_turn_types::canonical_conversation_root(&messages),
            projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };
        let bundle = astra_turn_types::ResumeBundleV1 {
            schema_version: astra_turn_types::RESUME_BUNDLE_SCHEMA_VERSION,
            cursor,
            source: astra_turn_types::ResumeSourceV1::CanonicalJournal,
            conversation_messages: messages,
            materialized_conversation_root_hash: None,
            degraded_reasons: Vec::new(),
            repair_actions: Vec::new(),
            projections: Default::default(),
        };

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
