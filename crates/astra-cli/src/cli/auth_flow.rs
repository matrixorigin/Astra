use crate::cli::cli_config::cli_utils::{
    CredentialStore, Profile, cli_profile_owner_scope, credential_store, load_credentials,
    map_thin_err, profile_name,
};
use crate::cli::session::session_state::SessionState;
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod browser_code;

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
    website_base: &str,
) -> Result<String, String> {
    do_memoria_browser_login_with_opener(api, profile, website_base, open_login_url).await
}

async fn do_memoria_browser_login_with_opener(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    website_base: &str,
    open: impl FnOnce(&str),
) -> Result<String, String> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let website = validate_login_website(website_base)?;
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
    let allowed_origin = website.origin().ascii_serialization();
    let verifier = browser_code::verifier();
    let connect_url = format!(
        "{}/connect/astra?port={port}&state={expected_state}&cli_version={}",
        website_base.trim_end_matches('/'),
        env!("CARGO_PKG_VERSION")
    );
    let connect_url = browser_code::append_capability(&connect_url, &verifier)?;
    eprintln!("Open this page to connect Astra:\n{connect_url}");
    open(&connect_url);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    let mut rejected = 0_u8;
    let mut requests = 0_u8;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("browser login timed out; run `astra login` to try again".to_string());
        }
        let (mut stream, _) = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| "browser login timed out; run `astra login` to try again".to_string())?
            .map_err(|error| format!("local login callback failed: {error}"))?;
        // Bound even harmless-looking traffic (OPTIONS/favicon/unknown paths).
        // This is resource containment, not protection against a hostile local host.
        requests += 1;
        // Read the bounded request before replying, including at the request
        // limit: closing a socket with unread request bytes can reset the peer
        // and discard the error response on some platforms.
        let request = tokio::time::timeout_at(deadline, read_callback_request(&mut stream))
            .await
            .map_err(|_| "browser login timed out; run `astra login` to try again")?;
        if requests > 64 {
            write_callback_response(
                &mut stream,
                "429 Too Many Requests",
                None,
                "too many requests",
                deadline,
            )
            .await;
            return Err("too many browser login requests; run `astra login` again".into());
        }
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                rejected = rejected.saturating_add(1);
                if error.is_navigation {
                    browser_code::write_result(&mut stream, false, deadline).await;
                } else {
                    write_callback_response(
                        &mut stream,
                        "400 Bad Request",
                        None,
                        "invalid request",
                        deadline,
                    )
                    .await;
                }
                if rejected >= 3 {
                    return Err("too many invalid browser login callbacks".to_string());
                }
                continue;
            }
        };
        if request.method == "GET" {
            let path = request.path.split('?').next().unwrap_or_default();
            if path != "/callback" {
                let (status, body) = if path == "/favicon.ico" {
                    ("204 No Content", "")
                } else {
                    ("404 Not Found", "not found")
                };
                write_callback_response(&mut stream, status, None, body, deadline).await;
                continue;
            }
            let code = match browser_code::callback_code(&request.path, &expected_state) {
                Ok(code) if request.body.is_empty() => code,
                _ => {
                    rejected = rejected.saturating_add(1);
                    browser_code::write_result(&mut stream, false, deadline).await;
                    if rejected >= 3 {
                        return Err("too many invalid browser login callbacks".into());
                    }
                    continue;
                }
            };
            let result = tokio::time::timeout_at(deadline, async {
                let key =
                    browser_code::redeem(website_base, &code, &verifier, port, &expected_state)
                        .await?;
                do_memoria_login_with_key(api, profile, &key).await
            })
            .await
            .map_err(|_| "Browser login timed out; run astra login again".to_string())?;
            browser_code::write_result(&mut stream, result.is_ok(), deadline).await;
            return result;
        }
        if request.method == "OPTIONS" {
            let origin = (request.origin.as_deref() == Some(allowed_origin.as_str()))
                .then_some(allowed_origin.as_str());
            write_callback_response(&mut stream, "204 No Content", origin, "", deadline).await;
            continue;
        }
        if request.method != "POST"
            || request.path != "/callback"
            || request.origin.as_deref() != Some(allowed_origin.as_str())
            || request.content_type.as_deref() != Some("application/json")
        {
            rejected = rejected.saturating_add(1);
            write_callback_response(
                &mut stream,
                "403 Forbidden",
                None,
                "callback rejected",
                deadline,
            )
            .await;
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
                    deadline,
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
                deadline,
            )
            .await;
            if rejected >= 3 {
                return Err("too many invalid browser login callbacks".to_string());
            }
            continue;
        }
        match tokio::time::timeout_at(
            deadline,
            do_memoria_login_with_key(api, profile, &callback.memoria_connection_key),
        )
        .await
        .map_err(|_| "Browser login timed out; run astra login again".to_string())?
        {
            Ok(token) => {
                write_callback_response(
                    &mut stream,
                    "200 OK",
                    Some(&allowed_origin),
                    r#"{"status":"connected"}"#,
                    deadline,
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
                    deadline,
                )
                .await;
                return Err(error);
            }
        }
    }
}

#[derive(Debug)]
struct CallbackRequest {
    method: String,
    path: String,
    origin: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct CallbackReadError {
    // Presentation hint only, never used to accept or authorize a callback.
    is_navigation: bool,
}

async fn read_callback_request(
    stream: &mut tokio::net::TcpStream,
) -> Result<CallbackRequest, CallbackReadError> {
    let mut is_navigation = false;
    tokio::time::timeout(
        Duration::from_secs(5),
        read_callback_request_inner(stream, &mut is_navigation),
    )
    .await
    .unwrap_or_else(|_| Err("callback read timed out".into()))
    .map_err(|_| CallbackReadError { is_navigation })
}

async fn read_callback_request_inner(
    stream: &mut tokio::net::TcpStream,
    is_navigation: &mut bool,
) -> Result<CallbackRequest, String> {
    let mut data = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("callback request is incomplete".into());
        }
        data.extend_from_slice(&chunk[..read]);
        *is_navigation = data.starts_with(b"GET /callback?") || data.starts_with(b"GET /callback ");
        if data.len() > 8192 {
            return Err("callback request is too large".into());
        }
        let Some(end) = data
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
        else {
            continue;
        };
        let (mut request, length) = parse_callback_headers(&data[..end])?;
        if data.len() < end + length {
            continue;
        }
        if data.len() != end + length {
            return Err("callback has trailing data".into());
        }
        request.body = data[end..].to_vec();
        return Ok(request);
    }
}

fn parse_callback_headers(data: &[u8]) -> Result<(CallbackRequest, usize), String> {
    let text = std::str::from_utf8(data).map_err(|_| "invalid callback headers")?;
    let mut lines = text.split("\r\n");
    let line = lines.next().ok_or("missing callback request line")?;
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 3 || !matches!(parts[2], "HTTP/1.1" | "HTTP/1.0") {
        return Err("invalid callback request line".into());
    }
    let mut origin = None;
    let mut content_type = None;
    let mut length = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or("invalid callback header")?;
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "origin" => {
                if origin.replace(value.to_string()).is_some() {
                    return Err("duplicate callback origin".into());
                }
            }
            "content-type" => {
                if content_type
                    .replace(
                        value
                            .split(';')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_ascii_lowercase(),
                    )
                    .is_some()
                {
                    return Err("duplicate callback content type".into());
                }
            }
            "content-length" => {
                let value = value
                    .parse::<usize>()
                    .map_err(|_| "invalid callback content length")?;
                if value > 4096 || length.replace(value).is_some() {
                    return Err("invalid or duplicate callback content length".into());
                }
            }
            "transfer-encoding" => return Err("callback transfer encoding is unsupported".into()),
            _ => {}
        }
    }
    if parts[0] == "POST" && length.is_none() {
        return Err("callback content length is required".into());
    }
    Ok((
        CallbackRequest {
            method: parts[0].into(),
            path: parts[1].into(),
            origin,
            content_type,
            body: vec![],
        },
        length.unwrap_or(0),
    ))
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    origin: Option<&str>,
    body: &str,
    deadline: tokio::time::Instant,
) {
    let cors = origin
        .map(|origin| {
            format!(
                "Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Private-Network: true\r\nVary: Origin\r\n"
            )
        })
        .unwrap_or_default();
    let body = if body.is_empty() {
        String::new()
    } else {
        serde_json::from_str::<serde_json::Value>(body)
            .unwrap_or_else(|_| serde_json::json!({"error": body}))
            .to_string()
    };
    let response = format!(
        "HTTP/1.1 {status}\r\n{cors}Content-Type: application/json; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if write_callback_bytes(stream, response.as_bytes(), deadline)
        .await
        .is_err()
    {
        tracing::warn!("could not deliver browser login callback response");
    }
}

async fn write_callback_bytes<W: tokio::io::AsyncWrite + Unpin>(
    stream: &mut W,
    response: &[u8],
    deadline: tokio::time::Instant,
) -> std::io::Result<()> {
    let write_deadline = deadline.min(tokio::time::Instant::now() + Duration::from_secs(2));
    tokio::time::timeout_at(write_deadline, async {
        stream.write_all(response).await?;
        stream.shutdown().await
    })
    .await
    .unwrap_or_else(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "callback write timed out",
        ))
    })
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
    let result = windows_browser_command(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "browser launch is unsupported",
    ));
    if let Err(error) = result {
        eprintln!("Could not open a browser automatically: {error}");
    }
}

pub(crate) async fn discover_login_website(
    api: &astra_thin_client::ThinClient,
) -> Result<Option<String>, String> {
    let methods = match api.get_auth_methods().await {
        Ok(value) => value,
        // Old/self-hosted servers keep the original password journey. Do not
        // silently downgrade on network errors, denied access or malformed JSON.
        Err(astra_thin_client::ThinClientError::Api { status, .. }) if status.as_u16() == 404 => {
            return Ok(None);
        }
        Err(error) => return Err(map_thin_err(error)),
    };
    #[derive(Deserialize)]
    struct Methods {
        password: bool,
        memoria: Option<BrowserProvider>,
    }
    #[derive(Deserialize)]
    struct BrowserProvider {
        issuer: String,
        authorization_url: String,
    }
    let methods: Methods =
        serde_json::from_value(methods).map_err(|_| "Invalid Server login configuration")?;
    if let Some(provider) = methods.memoria {
        if provider.issuer.trim().is_empty() {
            return Err("Server login issuer is missing".into());
        }
        validate_login_website(&provider.authorization_url)?;
        return Ok(Some(provider.authorization_url));
    }
    if methods.password {
        Ok(None)
    } else {
        Err("Server has no available login method".into())
    }
}

fn login_website_host_is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn validate_login_website(value: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(value).map_err(|_| "Invalid Server login URL")?;
    let loopback = url.host_str().is_some_and(login_website_host_is_loopback);
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.scheme() == "http" && !loopback)
    {
        return Err("Server login URL requires HTTPS (HTTP is allowed only on loopback)".into());
    }
    Ok(url)
}

#[cfg(any(target_os = "windows", test))]
fn windows_browser_command(url: &str) -> std::process::Command {
    let mut command = std::process::Command::new("powershell.exe");
    // The URL is data in the child environment, never shell source. In
    // particular the callback's &state= cannot become another command.
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Process -FilePath $env:ASTRA_LOGIN_URL",
        ])
        .env("ASTRA_LOGIN_URL", url);
    command
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
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[serial_test::serial]
    #[tokio::test]
    async fn browser_login_entrypoint_supports_local_codes_and_legacy_without_remote_polling() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};
        let _creds_guard = crate::tests::isolate_credentials();
        for legacy in [false, true] {
            let website = MockServer::start().await;
            let server = MockServer::start().await;
            Mock::given(method("POST")).and(path("/auth/memoria"))
                .and(body_json(json!({"connection_key":"test-connection-key"})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "user_id":"browser-user","access_token":"test-access","refresh_token":"test-refresh"})))
                .expect(1).mount(&server).await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let website_url = website.uri();
            let login =
                super::do_memoria_browser_login_with_opener(&api, None, &website_url, |url| {
                    sender.send(url.to_string()).unwrap();
                });
            let browser = async {
                let url = url::Url::parse(&receiver.await.unwrap()).unwrap();
                let fields: std::collections::HashMap<_, _> = url
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                assert_eq!(fields["callback_transport"], "authorization_code_v1");
                assert_eq!(fields["code_challenge_method"], "S256");
                assert!(!fields.contains_key("code_verifier"));
                let callback = format!("http://127.0.0.1:{}/callback", fields["port"]);
                let code = super::browser_code::verifier();
                let client = reqwest::Client::builder().no_proxy().build().unwrap();
                for (path, status) in [("/favicon.ico", 204), ("/wrong", 404)] {
                    let response = client
                        .get(format!("http://127.0.0.1:{}{path}", fields["port"]))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(response.status(), status);
                    assert!(!response.text().await.unwrap().contains("<html"));
                }
                // Cookies are shared across loopback ports. Oversized browser
                // headers must fail closed with a card, not echoed plaintext.
                let mut stream =
                    tokio::net::TcpStream::connect(format!("127.0.0.1:{}", fields["port"]))
                        .await
                        .unwrap();
                let request = format!(
                    "GET /callback?code={code}&state={} HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: secret-cookie={}\r\n\r\n",
                    fields["state"],
                    "x".repeat(8200)
                );
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut malformed_response = String::new();
                stream
                    .read_to_string(&mut malformed_response)
                    .await
                    .unwrap();
                assert!(malformed_response.starts_with("HTTP/1.1 400"));
                assert!(malformed_response.contains("Content-Type: text/html; charset=utf-8"));
                assert!(malformed_response.contains("class=\"card failure\""));
                for secret in [&code, &fields["state"], "secret-cookie"] {
                    assert!(!malformed_response.contains(secret));
                }
                // Unrelated/wrong-state requests must not cause credential exchange
                // or prevent the valid local browser from finishing afterwards.
                let invalid = client
                    .get(&callback)
                    .query(&[("code", &code), ("state", &"wrong".to_string())])
                    .send()
                    .await
                    .unwrap();
                assert_eq!(invalid.status(), 400);
                assert_eq!(
                    invalid.headers()["content-type"],
                    "text/html; charset=utf-8"
                );
                let failure_body = invalid.text().await.unwrap();
                assert!(failure_body.contains("class=\"card failure\""));
                for secret in [
                    &code,
                    &fields["state"],
                    "test-access",
                    "test-refresh",
                    "test-connection-key",
                ] {
                    assert!(!failure_body.contains(secret));
                }
                assert!(website.received_requests().await.unwrap().is_empty());
                assert!(server.received_requests().await.unwrap().is_empty());
                let response = if legacy {
                    client.post(&callback).header("Origin",website.uri())
                        .json(&json!({"state":fields["state"],"memoria_connection_key":"test-connection-key"}))
                        .send().await.unwrap()
                } else {
                    let challenge = fields["code_challenge"].clone();
                    let state = fields["state"].clone();
                    let expected_callback = callback.clone();
                    let expected_code = code.clone();
                    Mock::given(method("POST"))
                        .and(path("/api/auth/astra/browser-login/redeem"))
                        .respond_with(move |req: &wiremock::Request| {
                            let body: serde_json::Value = req.body_json().unwrap();
                            assert_eq!(body["authorization_code"], expected_code);
                            assert_eq!(body["state"], state);
                            assert_eq!(body["redirect_uri"], expected_callback);
                            let verifier = body["code_verifier"].as_str().unwrap();
                            assert_eq!(
                                URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                                challenge
                            );
                            ResponseTemplate::new(200)
                                .set_body_json(json!({"connection_key":"test-connection-key"}))
                        })
                        .expect(1)
                        .mount(&website)
                        .await;
                    client
                        .get(&callback)
                        .query(&[("code", &code), ("state", &fields["state"])])
                        .send()
                        .await
                        .unwrap()
                };
                assert!(response.status().is_success());
                assert_eq!(response.headers()["cache-control"], "no-store");
                if legacy {
                    assert_eq!(
                        response.headers()["content-type"],
                        "application/json; charset=utf-8"
                    );
                    assert_eq!(
                        response.headers()["access-control-allow-origin"],
                        website.uri()
                    );
                    assert_eq!(
                        response.json::<serde_json::Value>().await.unwrap(),
                        json!({"status":"connected"})
                    );
                } else {
                    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
                    assert_eq!(
                        response.headers()["content-type"],
                        "text/html; charset=utf-8"
                    );
                    let body = response.text().await.unwrap();
                    assert!(body.contains("You are signed in to Astra"));
                    assert!(body.contains("Return to your terminal to continue."));
                    for secret in [
                        &code,
                        &fields["state"],
                        "test-access",
                        "test-refresh",
                        "test-connection-key",
                    ] {
                        assert!(!body.contains(secret));
                    }
                }
                assert_eq!(
                    website.received_requests().await.unwrap().len(),
                    usize::from(!legacy)
                );
            };
            let (result, ()) = tokio::join!(login, browser);
            assert_eq!(result.unwrap(), "test-access");
            let profile = load_credentials().profiles.remove("default").unwrap();
            assert_eq!(profile.account_id.as_deref(), Some("browser-user"));
            assert_eq!(profile.access_token.as_deref(), Some("test-access"));
            assert_eq!(profile.refresh_token.as_deref(), Some("test-refresh"));
        }
    }

    #[tokio::test]
    async fn callback_rejections_and_unrelated_traffic_are_bounded() {
        for invalid_callback in [true, false] {
            let website = MockServer::start().await;
            let server = MockServer::start().await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let website_url = website.uri();
            let login =
                super::do_memoria_browser_login_with_opener(&api, None, &website_url, |url| {
                    sender.send(url.to_string()).unwrap();
                });
            let browser = async {
                let url = url::Url::parse(&receiver.await.unwrap()).unwrap();
                let port = url
                    .query_pairs()
                    .find(|(key, _)| key == "port")
                    .unwrap()
                    .1
                    .into_owned();
                let client = reqwest::Client::builder().no_proxy().build().unwrap();
                let count = if invalid_callback { 3 } else { 65 };
                for attempt in 1..=count {
                    if attempt == 65 {
                        let mut stream =
                            tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                                .await
                                .unwrap();
                        let mut byte = [0_u8; 1];
                        assert!(
                            tokio::time::timeout(Duration::from_millis(20), stream.read(&mut byte))
                                .await
                                .is_err(),
                            "the limit response must wait for the bounded request read"
                        );
                        stream
                            .write_all(b"GET /wrong HTTP/1.1\r\nHost: localhost\r\n\r\n")
                            .await
                            .unwrap();
                        let mut response = String::new();
                        stream.read_to_string(&mut response).await.unwrap();
                        assert!(response.starts_with("HTTP/1.1 429 Too Many Requests\r\n"));
                        assert!(response.contains("too many requests"));
                        continue;
                    }
                    let path = if invalid_callback {
                        "/callback?state=wrong"
                    } else {
                        "/wrong"
                    };
                    let response = client
                        .get(format!("http://127.0.0.1:{port}{path}"))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(
                        response.status().as_u16(),
                        if invalid_callback { 400 } else { 404 }
                    );
                }
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(login, browser)
            })
            .await
            .unwrap();
            assert!(result.unwrap_err().contains("too many"));
            assert!(website.received_requests().await.unwrap().is_empty());
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn browser_login_exchange_failure_returns_card_without_saving_or_echoing_secrets() {
        let _creds_guard = crate::tests::isolate_credentials();
        let website = MockServer::start().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/auth/astra/browser-login/redeem"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_json(json!({"detail":"private-upstream-error"})),
            )
            .expect(1)
            .mount(&website)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let website_url = website.uri();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let login = super::do_memoria_browser_login_with_opener(&api, None, &website_url, |url| {
            sender.send(url.to_string()).unwrap();
        });
        let browser = async {
            let url = url::Url::parse(&receiver.await.unwrap()).unwrap();
            let fields: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            let code = super::browser_code::verifier();
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("http://127.0.0.1:{}/callback", fields["port"]))
                .query(&[("code", &code), ("state", &fields["state"])])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400);
            assert_eq!(
                response.headers()["content-type"],
                "text/html; charset=utf-8"
            );
            let body = response.text().await.unwrap();
            assert!(body.contains("class=\"card failure\""));
            assert!(body.contains("Couldn’t complete sign-in"));
            for secret in [&code, &fields["state"], "private-upstream-error"] {
                assert!(!body.contains(secret));
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(login, browser)
        })
        .await
        .unwrap();
        assert!(result.unwrap_err().contains("unavailable"));
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(
            load_credentials()
                .profiles
                .values()
                .all(|profile| profile.access_token.is_none() && profile.refresh_token.is_none())
        );
    }

    #[tokio::test]
    async fn callback_writes_bound_stalled_peers_and_report_io_errors() {
        // A one-byte buffer deterministically stalls write_all without relying
        // on platform TCP window sizes or timing a real browser.
        let (mut writer, _reader) = tokio::io::duplex(1);
        let error = super::write_callback_bytes(
            &mut writer,
            b"response",
            tokio::time::Instant::now() + Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let (mut writer, _reader) = tokio::io::duplex(1);
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            super::write_callback_bytes(
                &mut writer,
                b"response",
                tokio::time::Instant::now() + Duration::from_secs(300),
            ),
        )
        .await
        .expect("the two-second write cap must apply independently of the login deadline")
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let (mut writer, reader) = tokio::io::duplex(1);
        drop(reader);
        let error = super::write_callback_bytes(
            &mut writer,
            b"response",
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        let (mut writer, mut reader) = tokio::io::duplex(64);
        super::write_callback_bytes(
            &mut writer,
            b"complete",
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        // Shutdown delivers EOF even while the writer remains in scope.
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(1), reader.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, "complete");
    }

    #[tokio::test]
    async fn malformed_legacy_request_still_receives_json_without_echoing_input() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let error = super::read_callback_request(&mut stream).await.unwrap_err();
            assert!(!error.is_navigation);
            super::write_callback_response(
                &mut stream,
                "400 Bad Request",
                None,
                "invalid request",
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await;
        };
        let client = async {
            let mut stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            stream
                .write_all(b"POST /callback HTTP/1.1\r\nX-Secret: private-cookie\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(headers.contains("application/json; charset=utf-8"));
            assert!(headers.contains("X-Content-Type-Options: nosniff"));
            assert!(headers.contains("Referrer-Policy: no-referrer"));
            assert!(!response.contains("private-cookie"));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(body).unwrap(),
                json!({"error":"invalid request"})
            );
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, client);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn browser_login_rejects_invalid_website_before_opening_browser() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let result = super::do_memoria_browser_login_with_opener(
            &api,
            None,
            "http://remote.invalid",
            |_| panic!("must not open browser"),
        )
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn loopback_callback_rejects_ambiguous_and_oversized_headers() {
        for headers in [
            "Content-Length: 1\r\nContent-Length: 2",
            "Content-Length: invalid",
            "Content-Length: 4097",
            "Content-Length: 0\r\nOrigin: https://a.example\r\nOrigin: https://b.example",
            "Transfer-Encoding: chunked",
        ] {
            let input = format!("POST /callback HTTP/1.1\r\n{headers}\r\n\r\n");
            assert!(
                super::parse_callback_headers(input.as_bytes()).is_err(),
                "{headers}"
            );
        }
    }

    #[tokio::test]
    async fn loopback_callback_rejects_truncated_body() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sender = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"POST /callback HTTP/1.1\r\nContent-Length: 10\r\n\r\n{}")
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(super::read_callback_request(&mut stream).await.is_err());
        sender.await.unwrap();
    }

    #[test]
    fn login_urls_require_https_except_loopback() {
        for good in [
            "https://thememoria.ai",
            "http://localhost",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
        ] {
            assert!(super::validate_login_website(good).is_ok(), "{good}");
        }
        for bad in [
            "http://thememoria.ai",
            "http://127.0.0.1.example",
            "javascript:alert(1)",
            "https://user:password@example.com",
            "https://example.com?x=1",
            "https://example.com#fragment",
        ] {
            assert!(super::validate_login_website(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn windows_browser_launch_keeps_callback_url_out_of_shell_source() {
        let url = "https://thememoria.ai/connect/astra?port=1234&state=abc&cli_version=0.2.1";
        let command = super::windows_browser_command(url);
        let args: Vec<_> = command
            .get_args()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter()
                .all(|arg| !arg.contains(url) && !arg.contains("&state"))
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "ASTRA_LOGIN_URL"
                    && value == Some(std::ffi::OsStr::new(url)))
        );
    }

    #[tokio::test]
    async fn login_discovery_preserves_local_servers_and_rejects_bad_cloud_urls() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        for (status, payload, expected) in [
            (404, serde_json::json!({}), "password"),
            (
                200,
                serde_json::json!({"password":true,"memoria":null}),
                "password",
            ),
            (
                200,
                serde_json::json!({"password":true,"memoria":{"issuer":"https://mem.example","authorization_url":"https://thememoria.ai"}}),
                "browser",
            ),
            (
                200,
                serde_json::json!({"password":true,"memoria":{"issuer":"https://mem.example","authorization_url":"http://remote.example"}}),
                "error",
            ),
            (503, serde_json::json!({}), "error"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/auth/methods"))
                .respond_with(ResponseTemplate::new(status).set_body_json(payload))
                .mount(&server)
                .await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let result = super::discover_login_website(&api).await;
            match expected {
                "password" => assert_eq!(result.unwrap(), None),
                "browser" => assert_eq!(result.unwrap().as_deref(), Some("https://thememoria.ai")),
                _ => assert!(result.is_err()),
            }
        }
    }

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
