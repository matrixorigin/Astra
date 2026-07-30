use crate::cli::cli_config::cli_utils::{
    CredentialStore, Profile, cli_profile_owner_scope, credential_store, load_credentials,
    map_thin_err, profile_name,
};
use crate::cli::session::session_state::SessionState;
use serde::Deserialize;
use std::time::Duration;

/// Session authentication failure that can be repaired by `/login`.
///
/// Excludes upstream model-provider credential failures. Those belong to the
/// provider config surface, not Astra session auth.
pub(crate) fn is_auth_error(error: &str) -> bool {
    if is_llm_provider_auth_error(error) {
        return false;
    }
    crate::cli::cli_config::cli_utils::is_astra_session_auth_error(error)
}

/// Detect upstream LLM provider authentication failures such as Bedrock or
/// Anthropic key problems. `/login` cannot repair these.
pub(crate) fn is_llm_provider_auth_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("llm provider authentication failed") || lower.contains("[auth] llm provider")
}

pub(crate) fn clear_profile_auth(profile: Option<&str>) -> Result<(), String> {
    credential_store()
        .mutate(|creds| {
            let name = profile_name(profile, creds);
            if let Some(entry) = creds.profiles.get_mut(&name) {
                entry.access_token = None;
                entry.refresh_token = None;
                entry.last_session_id = None;
            }
        })
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
pub(crate) struct AuthTokenPayload {
    pub(crate) user_id: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
}

pub(crate) fn parse_auth_tokens(body: &str) -> Result<AuthTokenPayload, String> {
    let tokens: AuthTokenPayload = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if tokens.access_token.is_empty() {
        return Err("missing access_token".to_string());
    }
    if tokens.refresh_token.is_empty() {
        return Err("missing refresh_token".to_string());
    }
    if tokens.user_id.trim().is_empty() {
        return Err("missing user_id".to_string());
    }
    Ok(tokens)
}

pub(crate) fn save_profile_auth_tokens(
    profile: Option<&str>,
    username: &str,
    tokens: &AuthTokenPayload,
) -> Result<(), String> {
    let username = username.to_string();
    let access = tokens.access_token.clone();
    let refresh = tokens.refresh_token.clone();
    let name = credential_store()
        .mutate(|creds| {
            let name =
                CredentialStore::resolve_profile_name(profile, creds.current_profile.as_deref());
            let existing = creds.profiles.get(&name).cloned().unwrap_or_default();
            let prev_session = if existing.account_id.as_deref() == Some(tokens.user_id.as_str()) {
                existing.last_session_id
            } else {
                None
            };
            let updated = Profile {
                username: Some(username.clone()),
                account_id: Some(tokens.user_id.clone()),
                access_token: Some(access.clone()),
                refresh_token: Some(refresh.clone()),
                last_session_id: prev_session,
                memoria_api_key: existing.memoria_api_key,
            };
            creds.current_profile = Some(name.clone());
            creds.profiles.insert(name.clone(), updated);
            name
        })
        .map_err(|e| e.to_string())?;
    crate::cli::cli_config::cli_utils::install_cli_profile_identity(
        name,
        Some(tokens.user_id.clone()),
    )
}

pub(crate) fn save_refreshed_profile_tokens(
    profile: Option<&str>,
    tokens: &AuthTokenPayload,
) -> Result<(), String> {
    let user_id = tokens.user_id.clone();
    let access = tokens.access_token.clone();
    let refresh = tokens.refresh_token.clone();
    credential_store()
        .mutate(|creds| {
            let name =
                CredentialStore::resolve_profile_name(profile, creds.current_profile.as_deref());
            let entry = creds.profiles.entry(name.clone()).or_default();
            match entry.account_id.as_deref() {
                Some(existing_account_id) if existing_account_id == user_id => {}
                Some(existing_account_id) => {
                    return Err(format!(
                    "refresh response account_id {user_id:?} does not match profile '{name}' account_id {existing_account_id:?}"
                ));
                }
                None => {
                    return Err(format!(
                        "profile '{name}' has no server-issued account_id; log in again instead of refreshing unbound credentials"
                    ));
                }
            }
            entry.access_token = Some(access.clone());
            entry.refresh_token = Some(refresh.clone());
            Ok(())
        })
        .map_err(|error| error.to_string())?
}

pub(crate) async fn do_login(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let tokens = request_login_tokens(api, username, password).await?;
    save_profile_auth_tokens(profile, username, &tokens)?;
    Ok(tokens.access_token)
}

async fn request_login_tokens(
    api: &astra_thin_client::ThinClient,
    username: &str,
    password: &str,
) -> Result<AuthTokenPayload, String> {
    let body = api
        .post_auth_login_json(&serde_json::json!({ "username": username, "password": password }))
        .await
        .map_err(map_thin_err)?;
    parse_auth_tokens(&body)
}

pub(crate) async fn do_register(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    username: &str,
    email: &str,
    password: &str,
) -> Result<String, String> {
    let tokens = request_register_tokens(api, username, email, password).await?;
    save_profile_auth_tokens(profile, username, &tokens)?;
    Ok(tokens.access_token)
}

async fn request_register_tokens(
    api: &astra_thin_client::ThinClient,
    username: &str,
    email: &str,
    password: &str,
) -> Result<AuthTokenPayload, String> {
    let body = api
        .post_auth_register_json(&serde_json::json!({
            "username": username,
            "email": email,
            "password": password,
        }))
        .await
        .map_err(map_thin_err)?;
    parse_auth_tokens(&body)
}

const AUTH_RUNTIME_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
const AUTH_RUNTIME_REPLACED_REASON: &str = "authentication runtime was replaced";

async fn retire_auth_runtime(state: &mut SessionState) {
    if let Some(spawner) = state.agent_spawner.take() {
        spawner
            .shutdown_and_wait_with_reason(AUTH_RUNTIME_SHUTDOWN_WAIT, AUTH_RUNTIME_REPLACED_REASON)
            .await;
    }
    state.delegation_engine = None;
    state.unregister_root_mailbox().await;
}

async fn prepare_session_auth_transition(
    profile: Option<&str>,
    account_id: &str,
    state: &mut SessionState,
) -> Result<(), String> {
    let credentials = load_credentials();
    let profile_name = profile_name(profile, &credentials);
    let target_owner = cli_profile_owner_scope(&profile_name, Some(account_id))?;
    let owner_changed = target_owner != astra_services::local_owner_scope();

    retire_auth_runtime(state).await;
    if owner_changed {
        // The old session must reach its durable boundary while the old owner
        // scope and credentials are still installed. Only then may local
        // ownerless APIs be rebound to the authenticated account.
        crate::cli::session::session_cleanup::finalize_session(state).await;
        state.reset_for_new_session();
        state.clear_session_id();
    }
    Ok(())
}

async fn initialize_authenticated_runtime(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    access_token: String,
    state: &mut SessionState,
) {
    crate::cli::agent_runtime::initialize_multi_agent_runtime(state, api, access_token, profile)
        .await;
}

pub(crate) async fn do_login_for_session(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    username: &str,
    password: &str,
    state: &mut SessionState,
) -> Result<String, String> {
    let tokens = request_login_tokens(api, username, password).await?;
    prepare_session_auth_transition(profile, &tokens.user_id, state).await?;
    save_profile_auth_tokens(profile, username, &tokens)?;
    let access_token = tokens.access_token;
    initialize_authenticated_runtime(api, profile, access_token.clone(), state).await;
    Ok(access_token)
}

pub(crate) async fn do_register_for_session(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    username: &str,
    email: &str,
    password: &str,
    state: &mut SessionState,
) -> Result<String, String> {
    let tokens = request_register_tokens(api, username, email, password).await?;
    prepare_session_auth_transition(profile, &tokens.user_id, state).await?;
    save_profile_auth_tokens(profile, username, &tokens)?;
    let access_token = tokens.access_token;
    initialize_authenticated_runtime(api, profile, access_token.clone(), state).await;
    Ok(access_token)
}

#[cfg(test)]
mod tests {
    use super::{
        AuthTokenPayload, clear_profile_auth, do_login, do_login_for_session, is_auth_error,
        is_llm_provider_auth_error, parse_auth_tokens, save_refreshed_profile_tokens,
    };
    use crate::cli::cli_config::cli_utils::{Profile, load_credentials, save_credentials};
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn auth_token_payload_requires_server_issued_user_identity() {
        let Err(missing) =
            parse_auth_tokens(r#"{"access_token":"access","refresh_token":"refresh"}"#)
        else {
            panic!("responses without user_id must not bind local ownership");
        };
        assert!(missing.contains("missing field `user_id`"), "{missing}");

        let Err(blank) = parse_auth_tokens(
            r#"{"user_id":"  ","access_token":"access","refresh_token":"refresh"}"#,
        ) else {
            panic!("blank user_id must not bind local ownership");
        };
        assert_eq!(blank, "missing user_id");
    }

    #[serial_test::serial]
    #[test]
    fn refresh_account_mismatch_is_atomic() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = load_credentials();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                account_id: Some("account-a".to_string()),
                access_token: Some("access-a".to_string()),
                refresh_token: Some("refresh-a".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let error = save_refreshed_profile_tokens(
            None,
            &AuthTokenPayload {
                user_id: "account-b".to_string(),
                access_token: "access-b".to_string(),
                refresh_token: "refresh-b".to_string(),
            },
        )
        .expect_err("refresh must not move a profile to another account");
        assert!(error.contains("does not match"), "{error}");

        let profile = load_credentials().profiles.remove("default").unwrap();
        assert_eq!(profile.account_id.as_deref(), Some("account-a"));
        assert_eq!(profile.access_token.as_deref(), Some("access-a"));
        assert_eq!(profile.refresh_token.as_deref(), Some("refresh-a"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn login_account_change_closes_old_owner_session_before_rebinding() {
        let _creds_guard = crate::tests::isolate_credentials();
        let (_sessions_dir, _journal_guard) = crate::tests::isolated_sessions_dir();
        let _identity_guard =
            crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
                "default", None,
            )
            .unwrap();
        let old_owner = astra_services::local_owner_scope();
        let session_id = "account-transition-session";
        let writer = astra_services::session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::session_start(
                    Some(session_id),
                    Some("model-a"),
                ),
            )
            .unwrap();
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.set_session_id(session_id);
        state.journal = Some(writer);
        state.turn = 1;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "user_id": "account-b",
                "access_token": "access-b",
                "refresh_token": "refresh-b"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let token = do_login_for_session(&api, None, "user-b", "password", &mut state)
            .await
            .unwrap();

        assert_eq!(token, "access-b");
        assert!(state.session_id.is_none());
        assert_ne!(astra_services::local_owner_scope(), old_owner);
        let old_events =
            astra_services::session_journal::read_journal_for_owner(&old_owner, session_id)
                .unwrap();
        assert!(old_events.iter().any(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::SessionEnd
        }));
        assert_eq!(
            load_credentials().profiles["default"].account_id.as_deref(),
            Some("account-b")
        );
        assert!(state.delegation_engine.is_some());
        assert!(state.agent_spawner.is_some());
    }

    #[test]
    fn auth_error_predicates_distinguish_provider_from_session() {
        let provider_msg = "LLM provider authentication failed";
        assert!(is_llm_provider_auth_error(provider_msg));
        assert!(!is_auth_error(provider_msg));

        let prefixed = "[auth] LLM provider rejected request: 401";
        assert!(is_llm_provider_auth_error(prefixed));
        assert!(!is_auth_error(prefixed));

        let session_msg =
            "API Error (401): Could not validate credentials\n  Hint: Session expired — try /login";
        assert!(!is_llm_provider_auth_error(session_msg));
        assert!(is_auth_error(session_msg));

        let unrelated_401 = "GitHub API Error: 401 Unauthorized";
        assert!(!is_llm_provider_auth_error(unrelated_401));
        assert!(
            !is_auth_error(unrelated_401),
            "generic upstream 401s must not be reported as Astra session expiry"
        );
    }

    #[serial_test::serial]
    #[test]
    fn clear_profile_auth_clears_tokens_and_last_session() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = load_credentials();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("tok".to_string()),
                refresh_token: Some("ref".to_string()),
                last_session_id: Some("sess-live".to_string()),
                memoria_api_key: Some("mem".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        clear_profile_auth(None).unwrap();

        let creds = load_credentials();
        let profile = &creds.profiles["default"];
        assert_eq!(profile.access_token, None);
        assert_eq!(profile.refresh_token, None);
        assert_eq!(profile.last_session_id, None);
        assert_eq!(profile.memoria_api_key.as_deref(), Some("mem"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn regular_login_uses_internal_endpoint() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/login"))
            .and(body_json(json!({
                "username": "astra-user",
                "password": "astra-pass"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "user_id": "astra-user-id",
                "access_token": "internal-access",
                "refresh_token": "internal-refresh"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let token = do_login(&api, None, "astra-user", "astra-pass")
            .await
            .unwrap();

        assert_eq!(token, "internal-access");
        let creds = load_credentials();
        let profile = &creds.profiles["default"];
        assert_eq!(profile.username.as_deref(), Some("astra-user"));
        assert_eq!(profile.access_token.as_deref(), Some("internal-access"));
        assert_eq!(profile.refresh_token.as_deref(), Some("internal-refresh"));
    }
}
