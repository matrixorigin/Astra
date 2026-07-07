use crate::cli::cli_config::cli_utils::{
    CredentialStore, Profile, credential_store, map_thin_err, profile_name,
};
use serde::Deserialize;

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
struct AuthTokenPayload {
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
pub(crate) struct ExternalProviderPayload {
    pub id: String,
    pub display_name: String,
    pub credential_type: String,
}

#[derive(Deserialize)]
struct ExternalProvidersPayload {
    providers: Vec<ExternalProviderPayload>,
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

pub(crate) async fn fetch_external_providers(
    api: &astra_thin_client::ThinClient,
) -> Result<Vec<ExternalProviderPayload>, String> {
    let body = api
        .get_auth_external_providers_text()
        .await
        .map_err(map_thin_err)?;
    let parsed: ExternalProvidersPayload =
        serde_json::from_str(&body).map_err(|e| e.to_string())?;
    Ok(parsed.providers)
}

pub(crate) async fn do_external_login(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    provider_id: &str,
    scope_id: Option<&str>,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let providers = fetch_external_providers(api).await?;
    let provider = providers.iter().find(|provider| provider.id == provider_id);
    let Some(provider) = provider else {
        return Err(format!(
            "external provider '{provider_id}' is not configured"
        ));
    };
    if provider.credential_type != "password" {
        return Err(format!(
            "external provider '{}' uses unsupported credential type '{}'",
            provider.id, provider.credential_type
        ));
    }

    let mut body = serde_json::json!({
        "provider_id": provider_id,
        "username": username,
        "password": password,
    });
    if let Some(scope_id) = scope_id {
        body["scope_id"] = serde_json::Value::String(scope_id.to_string());
    }
    let body = api
        .post_auth_external_login_json(&body)
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
    use super::{
        clear_profile_auth, do_external_login, do_login, is_auth_error, is_llm_provider_auth_error,
    };
    use crate::cli::cli_config::cli_utils::{Profile, load_credentials, save_credentials};
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
    async fn external_login_uses_provider_endpoints_and_saves_only_astra_tokens() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/external/providers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "providers": [
                    {"id": "moi", "display_name": "MOI", "credential_type": "password"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/external/login"))
            .and(body_json(json!({
                "provider_id": "moi",
                "username": "admin",
                "password": "admin"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "astra-access",
                "refresh_token": "astra-refresh"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let token = do_external_login(&api, None, "moi", None, "admin", "admin")
            .await
            .unwrap();

        assert_eq!(token, "astra-access");
        let creds = load_credentials();
        let profile = &creds.profiles["default"];
        assert_eq!(profile.username.as_deref(), Some("admin"));
        assert_eq!(profile.access_token.as_deref(), Some("astra-access"));
        assert_eq!(profile.refresh_token.as_deref(), Some("astra-refresh"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn regular_login_uses_internal_endpoint_without_external_provider_discovery() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/external/providers"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/login"))
            .and(body_json(json!({
                "username": "astra-user",
                "password": "astra-pass"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
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
