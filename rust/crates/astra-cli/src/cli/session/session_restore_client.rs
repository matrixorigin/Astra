use astra_services::session_restore::{
    HybridRestoreService, RestoredSession, ResumableSessionsResponse, SessionRestoreService,
};

use super::session_runtime;

fn validate_remote_session_id(session_id: &str) -> Result<(), String> {
    super::cli_utils::validate_cli_session_id(session_id)
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
        .restore_session(session_id)
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
        .restore_session(session_id)
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
        Ok(restored) => Ok(Some(restored)),
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
    use super::*;
    use crate::cli::cli_utils::{CredentialsFile, Profile, save_credentials};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mock_cloud_resumable_list(server: &MockServer, sessions: &[RestoredSession]) {
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ResumableSessionsResponse {
                    sessions: sessions.to_vec(),
                    limit: 20,
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
