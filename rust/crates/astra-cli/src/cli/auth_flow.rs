use super::*;
use crate::cli::cli_config::cli_utils::{CredentialStore, Profile, credential_store};

/// Session authentication failure that can be repaired by `/login`.
///
/// Excludes upstream model-provider credential failures. Those belong to the
/// provider config surface, not Astra session auth.
pub(crate) fn is_auth_error(error: &str) -> bool {
    if is_llm_provider_auth_error(error) {
        return false;
    }
    crate::cli::cli_utils::is_astra_session_auth_error(error)
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
struct AuthTokenPayload {
    access_token: String,
    refresh_token: String,
}

fn parse_auth_tokens(body: &str) -> Result<AuthTokenPayload, String> {
    let tokens: AuthTokenPayload = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if tokens.access_token.is_empty() {
        return Err("missing access_token".to_string());
    }
    if tokens.refresh_token.is_empty() {
        return Err("missing refresh_token".to_string());
    }
    Ok(tokens)
}

fn save_profile_auth_tokens(
    profile: Option<&str>,
    username: &str,
    tokens: &AuthTokenPayload,
) -> Result<(), String> {
    let username = username.to_string();
    let access = tokens.access_token.clone();
    let refresh = tokens.refresh_token.clone();
    credential_store()
        .mutate(|creds| {
            let name =
                CredentialStore::resolve_profile_name(profile, creds.current_profile.as_deref());
            let existing = creds.profiles.get(&name).cloned().unwrap_or_default();
            let prev_session = if existing.username.as_deref() == Some(&username) {
                existing.last_session_id
            } else {
                None
            };
            let updated = Profile {
                username: Some(username.clone()),
                access_token: Some(access.clone()),
                refresh_token: Some(refresh.clone()),
                last_session_id: prev_session,
                memoria_api_key: existing.memoria_api_key,
            };
            creds.current_profile = Some(name.clone());
            creds.profiles.insert(name, updated);
        })
        .map_err(|e| e.to_string())
}

pub(crate) async fn do_login(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let body = api
        .post_auth_login_json(&serde_json::json!({ "username": username, "password": password }))
        .await
        .map_err(map_thin_err)?;
    let tokens = parse_auth_tokens(&body)?;
    save_profile_auth_tokens(profile, username, &tokens)?;
    Ok(tokens.access_token)
}

pub(crate) async fn do_register(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    username: &str,
    email: &str,
    password: &str,
) -> Result<String, String> {
    let body = api
        .post_auth_register_json(&serde_json::json!({
            "username": username,
            "email": email,
            "password": password,
        }))
        .await
        .map_err(map_thin_err)?;
    let tokens = parse_auth_tokens(&body)?;
    save_profile_auth_tokens(profile, username, &tokens)?;
    Ok(tokens.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::cli_config::cli_utils::{load_credentials, save_credentials};

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
}
