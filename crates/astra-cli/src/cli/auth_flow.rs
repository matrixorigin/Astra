use crate::cli::cli_config::cli_utils::{
    CredentialStore, Profile, cli_profile_owner_scope, credential_store, load_credentials,
    map_thin_err, profile_name,
};
use crate::cli::session::session_state::SessionState;
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

pub(crate) async fn do_memoria_login_with_key(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    connection_key: &str,
) -> Result<String, String> {
    let tokens = request_memoria_tokens(api, connection_key).await?;
    save_profile_auth_tokens(profile, "memoria", &tokens)?;
    credential_store()
        .mutate(|creds| {
            let name =
                CredentialStore::resolve_profile_name(profile, creds.current_profile.as_deref());
            if let Some(entry) = creds.profiles.get_mut(&name) {
                // The connection key belongs only on the Astra server. Older
                // CLI versions stored a Memoria key here, so clear it during
                // the migration login as well.
                entry.memoria_api_key = None;
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(tokens.access_token)
}

async fn request_memoria_tokens(
    api: &astra_thin_client::ThinClient,
    connection_key: &str,
) -> Result<AuthTokenPayload, String> {
    let body = api
        .post_auth_memoria_json(&serde_json::json!({ "connection_key": connection_key }))
        .await
        .map_err(map_thin_err)?;
    parse_auth_tokens(&body)
}

#[derive(Deserialize)]
struct MemoriaConnectionCallback {
    state: String,
    memoria_connection_key: String,
}

pub(crate) async fn do_memoria_browser_login(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Result<String, String> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("failed to start local login callback: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("failed to inspect local login callback: {error}"))?
        .port();
    let mut state_bytes = [0_u8; 32];
    state_bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    state_bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let expected_state = URL_SAFE_NO_PAD.encode(state_bytes);
    let website_base =
        std::env::var("MEMORIA_WEB_URL").unwrap_or_else(|_| "https://thememoria.ai".to_string());
    let website = url::Url::parse(&website_base)
        .map_err(|_| "MEMORIA_WEB_URL must be an absolute HTTP(S) URL".to_string())?;
    if website.scheme() != "http" && website.scheme() != "https" {
        return Err("MEMORIA_WEB_URL must use HTTP or HTTPS".to_string());
    }
    let allowed_origin = website.origin().ascii_serialization();
    let connect_url = format!(
        "{}/connect/astra?port={port}&state={expected_state}&cli_version={}",
        website_base.trim_end_matches('/'),
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("Open this page to connect Astra:\n{connect_url}");
    open_login_url(&connect_url);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    let mut rejected = 0_u8;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("browser login timed out; run `astra login` to try again".to_string());
        }
        let (mut stream, _) = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| "browser login timed out; run `astra login` to try again".to_string())?
            .map_err(|error| format!("local login callback failed: {error}"))?;
        let request = read_callback_request(&mut stream).await;
        let Ok(request) = request else {
            rejected = rejected.saturating_add(1);
            write_callback_response(&mut stream, "400 Bad Request", None, "invalid request").await;
            if rejected >= 3 {
                return Err("too many invalid browser login callbacks".to_string());
            }
            continue;
        };
        if request.method == "OPTIONS" {
            let origin = (request.origin.as_deref() == Some(allowed_origin.as_str()))
                .then_some(allowed_origin.as_str());
            write_callback_response(&mut stream, "204 No Content", origin, "").await;
            continue;
        }
        if request.method != "POST"
            || request.path != "/callback"
            || request.origin.as_deref() != Some(allowed_origin.as_str())
            || request.content_type.as_deref() != Some("application/json")
        {
            rejected = rejected.saturating_add(1);
            write_callback_response(&mut stream, "403 Forbidden", None, "callback rejected").await;
            if rejected >= 3 {
                return Err("too many invalid browser login callbacks".to_string());
            }
            continue;
        }
        let callback: MemoriaConnectionCallback = match serde_json::from_slice(&request.body) {
            Ok(callback) => callback,
            Err(_) => {
                rejected = rejected.saturating_add(1);
                write_callback_response(
                    &mut stream,
                    "400 Bad Request",
                    Some(&allowed_origin),
                    "invalid callback",
                )
                .await;
                if rejected >= 3 {
                    return Err("too many invalid browser login callbacks".to_string());
                }
                continue;
            }
        };
        if !constant_time_eq(callback.state.as_bytes(), expected_state.as_bytes())
            || callback.memoria_connection_key.is_empty()
            || callback.memoria_connection_key.len() > 4096
        {
            rejected = rejected.saturating_add(1);
            write_callback_response(
                &mut stream,
                "403 Forbidden",
                Some(&allowed_origin),
                "callback rejected",
            )
            .await;
            if rejected >= 3 {
                return Err("too many invalid browser login callbacks".to_string());
            }
            continue;
        }
        match do_memoria_login_with_key(api, profile, &callback.memoria_connection_key).await {
            Ok(token) => {
                write_callback_response(
                    &mut stream,
                    "200 OK",
                    Some(&allowed_origin),
                    r#"{"status":"connected"}"#,
                )
                .await;
                return Ok(token);
            }
            Err(error) => {
                write_callback_response(
                    &mut stream,
                    "502 Bad Gateway",
                    Some(&allowed_origin),
                    "Astra could not verify the connection key.",
                )
                .await;
                return Err(error);
            }
        }
    }
}

struct CallbackRequest {
    method: String,
    path: String,
    origin: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

async fn read_callback_request(
    stream: &mut tokio::net::TcpStream,
) -> Result<CallbackRequest, String> {
    let mut data = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .map_err(|_| "callback read timed out".to_string())?
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..read]);
        if data.len() > 8192 {
            return Err("callback request is too large".to_string());
        }
        if let Some(header_end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = header_end + 4;
            let headers = std::str::from_utf8(&data[..header_end])
                .map_err(|_| "callback headers are invalid".to_string())?;
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            if content_length > 4096 {
                return Err("callback body is too large".to_string());
            }
            if data.len() >= header_end + content_length {
                break;
            }
        }
    }
    let header_end = data
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "callback headers are incomplete".to_string())?
        + 4;
    let headers = std::str::from_utf8(&data[..header_end])
        .map_err(|_| "callback headers are invalid".to_string())?;
    let mut lines = headers.lines();
    let mut request_line = lines
        .next()
        .ok_or_else(|| "callback request line is missing".to_string())?
        .split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();
    let origin = lines.find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            name.eq_ignore_ascii_case("origin")
                .then(|| value.trim().to_string())
        })
    });
    let content_type = headers.lines().find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            name.eq_ignore_ascii_case("content-type").then(|| {
                value
                    .trim()
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
        })
    });
    Ok(CallbackRequest {
        method,
        path,
        origin,
        content_type,
        body: data[header_end..].to_vec(),
    })
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    origin: Option<&str>,
    body: &str,
) {
    let cors = origin
        .map(|origin| {
            format!(
                "Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Private-Network: true\r\nVary: Origin\r\n"
            )
        })
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status}\r\n{cors}Content-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn open_login_url(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "browser launch is unsupported",
    ));
    if let Err(error) = result {
        eprintln!("Could not open a browser automatically: {error}");
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreparedAuthTransition {
    owner_changed: bool,
    runtime_needs_initialization: bool,
}

async fn prepare_session_auth_transition(
    profile: Option<&str>,
    account_id: &str,
    state: &mut SessionState,
) -> Result<PreparedAuthTransition, String> {
    let credentials = load_credentials();
    let profile_name = profile_name(profile, &credentials);
    let target_owner = cli_profile_owner_scope(&profile_name, Some(account_id))?;
    let owner_changed = target_owner != astra_services::local_owner_scope();
    let runtime_needs_initialization =
        state.agent_spawner.is_none() || state.delegation_engine.is_none();

    if owner_changed {
        // The old session must reach its durable boundary while the old owner
        // scope and credentials are still installed. Only then may local
        // ownerless APIs be rebound to the authenticated account.
        retire_auth_runtime(state).await;
        crate::cli::session::session_cleanup::finalize_session(state).await;
        state.reset_for_new_session();
        state.clear_session_id();
    } else if runtime_needs_initialization {
        // A same-owner login after `/logout`, or a partially initialized
        // runtime, is not a session boundary. Retire any incomplete half and
        // rebuild it after the new credentials have been saved.
        retire_auth_runtime(state).await;
    }
    Ok(PreparedAuthTransition {
        owner_changed,
        runtime_needs_initialization: owner_changed || runtime_needs_initialization,
    })
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
    let transition = prepare_session_auth_transition(profile, &tokens.user_id, state).await?;
    tracing::debug!(
        owner_changed = transition.owner_changed,
        runtime_needs_initialization = transition.runtime_needs_initialization,
        "prepared authenticated session transition"
    );
    save_profile_auth_tokens(profile, username, &tokens)?;
    let access_token = tokens.access_token.clone();
    if transition.runtime_needs_initialization {
        initialize_authenticated_runtime(api, profile, access_token.clone(), state).await;
    }
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
    let transition = prepare_session_auth_transition(profile, &tokens.user_id, state).await?;
    tracing::debug!(
        owner_changed = transition.owner_changed,
        runtime_needs_initialization = transition.runtime_needs_initialization,
        "prepared authenticated session transition"
    );
    save_profile_auth_tokens(profile, username, &tokens)?;
    let access_token = tokens.access_token;
    if transition.runtime_needs_initialization {
        initialize_authenticated_runtime(api, profile, access_token.clone(), state).await;
    }
    Ok(access_token)
}

#[cfg(test)]
mod tests {
    use super::{
        AuthTokenPayload, clear_profile_auth, do_login, do_login_for_session,
        do_memoria_login_with_key, is_auth_error, is_llm_provider_auth_error, parse_auth_tokens,
        read_callback_request, save_refreshed_profile_tokens,
    };
    use crate::cli::cli_config::cli_utils::{Profile, load_credentials, save_credentials};
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn loopback_callback_parser_reads_origin_content_type_and_secret_body() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let body = r#"{"state":"abc","memoria_connection_key":"secret"}"#;
            let request = format!(
                "POST /callback HTTP/1.1\r\nOrigin: https://thememoria.ai\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(request.as_bytes()).await.unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_callback_request(&mut stream).await.unwrap();
        client.await.unwrap();

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/callback");
        assert_eq!(request.origin.as_deref(), Some("https://thememoria.ai"));
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            json!({"state": "abc", "memoria_connection_key": "secret"})
        );
    }

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

    #[serial_test::serial]
    #[tokio::test]
    async fn same_owner_login_rebuilds_missing_runtime_without_resetting_session() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _identity_guard =
            crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
                "default",
                Some("account-a"),
            )
            .unwrap();
        let owner = astra_services::local_owner_scope();
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.set_session_id("same-owner-session");
        assert!(state.agent_spawner.is_none());
        assert!(state.delegation_engine.is_none());

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "user_id": "account-a",
                "access_token": "access-new",
                "refresh_token": "refresh-new"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let token = do_login_for_session(&api, None, "user-a", "password", &mut state)
            .await
            .unwrap();

        assert_eq!(token, "access-new");
        assert_eq!(astra_services::local_owner_scope(), owner);
        assert_eq!(state.session_id.as_deref(), Some("same-owner-session"));
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

    #[serial_test::serial]
    #[tokio::test]
    async fn memoria_login_sends_key_once_and_does_not_persist_it() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = load_credentials();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                memoria_api_key: Some("legacy-key".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/memoria"))
            .and(body_json(json!({"connection_key": "scoped-key"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "user_id": "memoria-user-id",
                "access_token": "astra-access",
                "refresh_token": "astra-refresh",
                "memory_access": "read_only",
                "granted_scopes": ["identity:read", "memory:read"]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let token = do_memoria_login_with_key(&api, None, "scoped-key")
            .await
            .unwrap();

        assert_eq!(token, "astra-access");
        let creds = load_credentials();
        let profile = &creds.profiles["default"];
        assert_eq!(profile.account_id.as_deref(), Some("memoria-user-id"));
        assert_eq!(profile.username.as_deref(), Some("memoria"));
        assert_eq!(profile.memoria_api_key, None);
    }
}
