use astra_credentials::native::{self, Environment, NativeSession, NativeStore, TokenResponse};
use astra_services::auth::uc::UcDiscovery;
use serde::Deserialize;
use std::collections::BTreeMap;

pub(crate) async fn login_with_observer(
    api: &astra_thin_client::ThinClient,
    discovery: UcDiscovery,
    observer: super::LoginObserver,
    terminal_workspace_prompt: bool,
) -> Result<(), String> {
    let store = NativeStore::new()?;
    native::secure_url(&api.api_origin())?;
    native::secure_url(&discovery.issuer)?;
    native::secure_url(&discovery.moi_api_url)?;
    if discovery.client_id != "astra-cli"
        || discovery.resource != "astra-api"
        || discovery.scope != "openid profile email astra:user aistudio:user"
    {
        return Err("unsupported UC native client contract".into());
    }
    let client = native::http_client()?;
    let response = client
        .get(format!(
            "{}/.well-known/openid-configuration",
            discovery.issuer
        ))
        .send()
        .await
        .map_err(|_| "UC discovery unavailable")?;
    if !response.status().is_success() {
        return Err("UC discovery rejected".into());
    }
    #[derive(Deserialize)]
    struct Metadata {
        issuer: String,
        authorization_endpoint: String,
        token_endpoint: String,
        revocation_endpoint: String,
        jwks_uri: String,
    }
    let metadata: Metadata = native::bounded_json(response).await?;
    if metadata.issuer != discovery.issuer {
        return Err("UC discovery issuer mismatch".into());
    }
    let environment = Environment {
        issuer: metadata.issuer,
        astra_url: api.api_origin(),
        moi_url: discovery.moi_api_url,
        authorization_endpoint: metadata.authorization_endpoint,
        token_endpoint: metadata.token_endpoint,
        revocation_endpoint: metadata.revocation_endpoint,
        jwks_uri: metadata.jwks_uri,
    };
    environment.validate()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "cannot start login callback")?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}/callback",
        listener
            .local_addr()
            .map_err(|_| "cannot inspect callback")?
            .port()
    );
    let state = super::browser_code::verifier();
    let nonce = super::browser_code::verifier();
    let verifier = super::browser_code::verifier();
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut authorize = native::secure_url(&environment.authorization_endpoint)?;
    authorize.query_pairs_mut().extend_pairs([
        ("client_id", "astra-cli"),
        ("response_type", "code"),
        ("scope", &discovery.scope),
        ("resource", "astra-api"),
        ("redirect_uri", &redirect_uri),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    observer(super::LoginProgress::OpenBrowser(authorize.to_string()))?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    for _ in 0..32 {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| "UC login timed out")?
            .map_err(|_| "UC callback unavailable")?;
        let request = tokio::time::timeout_at(deadline, super::read_callback_request(&mut stream))
            .await
            .map_err(|_| "UC login timed out")?;
        let callback = request.ok().and_then(|request| {
            callback_code(
                &request.method,
                &request.path,
                &request.body,
                &state,
                &environment.issuer,
            )
            .ok()
        });
        let Some(callback) = callback else {
            super::browser_code::write_result(&mut stream, false, deadline).await;
            continue;
        };
        let Callback::Code(code) = callback else {
            super::browser_code::write_result(&mut stream, false, deadline).await;
            return Err(
                "UC authorization was denied or cancelled; no local session was changed".into(),
            );
        };
        // Do not display a success page until both product bootstraps and the
        // local atomic commit have succeeded.
        observer(super::LoginProgress::Completing)?;
        let result = tokio::time::timeout_at(
            deadline,
            complete(
                &client,
                &environment,
                &redirect_uri,
                &code,
                &verifier,
                &nonce,
            ),
        )
        .await
        .map_err(|_| "UC login timed out".to_string())
        .and_then(|result| result);
        let result = match result {
            Ok((session, bootstrap)) => publish_prepared_login(&store, session, bootstrap).await,
            Err(error) => Err(error),
        };
        super::browser_code::write_result(
            &mut stream,
            result.is_ok(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await;
        let (session, bootstrap) = result?;
        // Login is already committed. Workspace selection is optional, and a
        // queue/list/prompt failure cannot undo authentication success.
        if terminal_workspace_prompt
            && let Err(error) = choose_workspace(&store, &session, bootstrap, None)
        {
            eprintln!("Workspace not selected: {error}. Use `astra auth workspace` later.");
        }
        return Ok(());
    }
    Err("too many invalid UC callbacks".into())
}

#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Code(String),
    Denied,
}

fn callback_code(
    method: &str,
    target: &str,
    body: &[u8],
    expected_state: &str,
    issuer: &str,
) -> Result<Callback, String> {
    if method != "GET" || !body.is_empty() || !target.starts_with("/callback?") {
        return Err("invalid callback method/path".into());
    }
    let url = url::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| "invalid callback URL")?;
    if url.path() != "/callback" || url.fragment().is_some() {
        return Err("invalid callback path".into());
    }
    let mut fields = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if !matches!(
            key.as_ref(),
            "code" | "state" | "iss" | "session_state" | "error" | "error_description"
        ) || fields
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err("invalid callback parameters".into());
        }
    }
    if !fields
        .get("state")
        .is_some_and(|v| super::constant_time_eq(v.as_bytes(), expected_state.as_bytes()))
        || fields.get("iss").is_some_and(|v| v != issuer)
    {
        return Err("login callback rejected".into());
    }
    if let Some(error) = fields.get("error") {
        if error.is_empty() || fields.contains_key("code") {
            return Err("invalid OAuth error callback".into());
        }
        return Ok(Callback::Denied);
    }
    fields
        .remove("code")
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .map(Callback::Code)
        .ok_or_else(|| "missing authorization code".into())
}

async fn complete(
    client: &reqwest::Client,
    environment: &Environment,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
    nonce: &str,
) -> Result<(NativeSession, MoiBootstrap), String> {
    let response = client
        .post(&environment.token_endpoint)
        .form(&[
            ("client_id", "astra-cli"),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
            ("code", code),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|_| "UC token exchange unavailable")?;
    if !response.status().is_success() {
        return Err("UC token exchange rejected".into());
    }
    let tokens: TokenResponse = native::bounded_json(response).await?;
    let result = prepare(client, environment, &tokens, nonce).await;
    if result.is_err() && !tokens.refresh_token.is_empty() {
        let _ = native::revoke(environment, &tokens.refresh_token).await;
    }
    result
}

async fn prepare(
    client: &reqwest::Client,
    environment: &Environment,
    tokens: &TokenResponse,
    nonce: &str,
) -> Result<(NativeSession, MoiBootstrap), String> {
    if tokens.access_token.is_empty()
        || tokens.refresh_token.is_empty()
        || !tokens.token_type.eq_ignore_ascii_case("bearer")
        || tokens.expires_in <= 0
        || tokens.expires_in > 86400
    {
        return Err("invalid UC token response".into());
    }
    let subject = verify_id_token(
        client,
        environment,
        tokens.id_token.as_deref().ok_or("missing UC ID token")?,
        nonce,
    )
    .await?;
    let (astra, moi) =
        prepare_products(client, environment, &tokens.access_token, &subject).await?;
    let session = NativeSession {
        environment: environment.clone(),
        generation: String::new(),
        subject,
        session_id: astra.session_id,
        astra_user_id: astra.user_id,
        moi_principal_id: moi.principal_id.clone(),
        catalog_user_id: moi.catalog_user_id.clone(),
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        expires_at: native::unix_now()? + tokens.expires_in,
        workspace_id: None,
        role_id: None,
        refresh_pending: false,
    };
    Ok((session, moi))
}

async fn publish_prepared_login(
    store: &NativeStore,
    session: NativeSession,
    bootstrap: MoiBootstrap,
) -> Result<(NativeSession, MoiBootstrap), String> {
    // The browser deadline has ended. Publication is the success boundary;
    // neither revocation nor result-page delivery may reverse it.
    let (published, old) = store.publish(session)?;
    if let Some(old) = old {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            native::revoke(&old.environment, &old.refresh_token),
        )
        .await
        {
            Ok(Ok(())) => (),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "previous UC session revocation failed")
            }
            Err(_) => tracing::warn!("previous UC session revocation timed out"),
        }
    }
    Ok((published, bootstrap))
}

#[derive(Deserialize)]
struct AstraBootstrap {
    issuer: String,
    subject: String,
    user_id: String,
    session_id: String,
}

async fn prepare_products(
    client: &reqwest::Client,
    environment: &Environment,
    token: &str,
    subject: &str,
) -> Result<(AstraBootstrap, MoiBootstrap), String> {
    // Establish the MOI account mapping before Astra reads the shared product
    // model policy with its server-owned runtime PAT. Workspace creation stays
    // asynchronous and is not a prerequisite for this account-level read.
    let moi = moi_bootstrap(client, environment, token).await?;
    if moi.issuer != environment.issuer || moi.subject != subject {
        return Err("product login identity mismatch".into());
    }
    let response = client
        .post(format!("{}/auth/uc/bootstrap", environment.astra_url))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| "Astra login preparation unavailable")?;
    if !response.status().is_success() {
        return Err(product_error(response, "Astra").await);
    }
    let astra: AstraBootstrap = native::bounded_json(response).await?;
    if astra.issuer != environment.issuer
        || astra.subject != subject
        || astra.session_id != moi.session_id
    {
        return Err("product login identity mismatch".into());
    }
    Ok((astra, moi))
}

#[derive(Deserialize)]
pub(crate) struct MoiBootstrap {
    pub issuer: String,
    pub subject: String,
    pub session_id: String,
    principal_id: String,
    catalog_user_id: String,
    workspaces: Vec<Workspace>,
    workspace_initialization: WorkspaceInitialization,
}

#[derive(Deserialize)]
struct Workspace {
    id: String,
    name: String,
    status: String,
    access_status: String,
}

#[derive(Deserialize)]
struct WorkspaceInitialization {
    status: String,
    code: Option<String>,
    message: Option<String>,
}

pub(crate) async fn moi_bootstrap(
    client: &reqwest::Client,
    environment: &Environment,
    token: &str,
) -> Result<MoiBootstrap, String> {
    #[derive(Deserialize)]
    struct Envelope {
        code: String,
        data: MoiBootstrap,
    }
    let response = client
        .post(format!("{}/auth/cli/bootstrap", environment.moi_url))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| "MOI login preparation unavailable")?;
    if !response.status().is_success() {
        return Err(product_error(response, "MOI").await);
    }
    let envelope: Envelope = native::bounded_json(response).await?;
    if envelope.code != "OK" {
        return Err("invalid MOI bootstrap response".into());
    }
    Ok(envelope.data)
}

async fn product_error(response: reqwest::Response, product: &str) -> String {
    let status = response.status();
    match native::bounded_json::<serde_json::Value>(response).await {
        Ok(value) => {
            let code = value
                .get("code")
                .or_else(|| value.get("error_code"))
                .and_then(|v| v.as_str())
                .unwrap_or("invalid_error_response");
            let message = value
                .get("msg")
                .or_else(|| value.get("detail"))
                .and_then(|v| v.as_str())
                .unwrap_or("Missing product error message");
            format!(
                "{product} ({status}, {}): {}",
                terminal_text(code),
                terminal_text(message)
            )
        }
        Err(_) => format!("{product} ({status}): invalid product error response"),
    }
}

fn terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|v| !v.is_control())
        .take(1024)
        .collect()
}

pub(crate) fn choose_workspace(
    store: &NativeStore,
    session: &NativeSession,
    bootstrap: MoiBootstrap,
    selected: Option<&str>,
) -> Result<(), String> {
    use std::io::IsTerminal;
    let choices: Vec<_> = bootstrap
        .workspaces
        .into_iter()
        .filter(|v| v.status == "ACTIVE" && v.access_status == "accepted")
        .collect();
    let initialization = bootstrap.workspace_initialization;
    if let Some(id) = selected {
        if !choices.iter().any(|v| v.id == id) {
            return Err("workspace is not an active membership of this MOI account".into());
        }
        store.select_workspace(session, Some(id))?;
        eprintln!("MOI workspace selected: {}", terminal_text(id));
        return Ok(());
    }
    if choices.is_empty() {
        eprintln!(
            "Workspace initialization: {}. {} {}",
            terminal_text(&initialization.status),
            terminal_text(initialization.code.as_deref().unwrap_or("")),
            terminal_text(initialization.message.as_deref().unwrap_or(""))
        );
        eprintln!(
            "Authentication is complete; use `astra auth workspace` when a workspace is ready."
        );
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    let mut labels = vec!["Skip for now".to_string()];
    labels.extend(
        choices
            .iter()
            .map(|v| format!("{} ({})", terminal_text(&v.name), terminal_text(&v.id))),
    );
    match inquire::Select::new("Select a MOI workspace", labels.clone()).prompt() {
        Ok(label) => {
            let index = labels
                .iter()
                .position(|v| v == &label)
                .ok_or("invalid workspace choice")?;
            if index > 0 {
                store.select_workspace(session, Some(&choices[index - 1].id))?;
            }
            Ok(())
        }
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(()),
        Err(_) => Err("workspace selection prompt unavailable".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn committed_login_survives_slow_previous_revocation_after_browser_deadline() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
        let server = MockServer::start().await;
        let issuer = server.uri();
        let root = tempfile::tempdir().unwrap();
        let store = NativeStore::with_directory(root.path().join("auth"));
        let original = NativeSession {
            environment: Environment {
                issuer: issuer.clone(),
                astra_url: server.uri(),
                moi_url: format!("{issuer}/newmoi"),
                authorization_endpoint: format!("{issuer}/authorize"),
                token_endpoint: format!("{issuer}/protocol/openid-connect/token"),
                revocation_endpoint: format!("{issuer}/protocol/openid-connect/revoke"),
                jwks_uri: format!("{issuer}/protocol/openid-connect/certs"),
            },
            generation: String::new(),
            subject: "user".into(),
            session_id: "old-session".into(),
            astra_user_id: "astra-user".into(),
            moi_principal_id: "moi-user".into(),
            catalog_user_id: "catalog-user".into(),
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            expires_at: native::unix_now().unwrap() + 3600,
            workspace_id: None,
            role_id: None,
            refresh_pending: false,
        };
        let (old, _) = store.publish(original.clone()).unwrap();
        Mock::given(path("/protocol/openid-connect/revoke"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(3)))
            .expect(1)
            .mount(&server)
            .await;
        let mut prepared = original;
        prepared.session_id = "new-session".into();
        prepared.access_token = "new-access".into();
        prepared.refresh_token = "new-refresh".into();
        let bootstrap = serde_json::from_value(serde_json::json!({
            "issuer":issuer, "subject":"user", "session_id":"new-session", "principal_id":"moi-user",
            "catalog_user_id":"catalog-user", "workspaces":[], "workspace_initialization":{"status":"pending"}
        })).unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(10);
        let (published, _) = publish_prepared_login(&store, prepared, bootstrap)
            .await
            .unwrap();
        assert!(tokio::time::Instant::now() > deadline);
        assert_ne!(published.generation, old.generation);
        assert_eq!(store.current().unwrap().generation, published.generation);
        assert_eq!(
            store
                .credential("astra", Some(&published.generation))
                .await
                .unwrap()
                .access_token,
            "new-access"
        );
    }

    #[tokio::test]
    async fn native_bootstrap_prepares_moi_before_astra_without_waiting_for_workspace() {
        use serde_json::json;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path},
        };

        for failure in [
            None,
            Some("moi"),
            Some("identity"),
            Some("astra"),
            Some("session"),
        ] {
            let server = MockServer::start().await;
            let ready = Arc::new(AtomicBool::new(false));
            let environment = Environment {
                issuer: "https://uc.example.test/realms/moi".into(),
                astra_url: server.uri(),
                moi_url: format!("{}/newmoi", server.uri()),
                authorization_endpoint: String::new(),
                token_endpoint: String::new(),
                revocation_endpoint: String::new(),
                jwks_uri: String::new(),
            };
            let moi_ready = ready.clone();
            let issuer = environment.issuer.clone();
            Mock::given(method("POST")).and(path("/newmoi/auth/cli/bootstrap"))
                .and(header("Authorization", "Bearer synthetic-native-token"))
                .respond_with(move |_: &wiremock::Request| {
                    if failure == Some("moi") { return ResponseTemplate::new(503); }
                    moi_ready.store(true, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(json!({"code":"OK", "data": {
                        "issuer": issuer, "subject": if failure == Some("identity") {"other"} else {"user"},
                        "session_id":"session", "principal_id":"principal", "catalog_user_id":"catalog-user",
                        "workspaces": [], "workspace_initialization": {"status":"pending"}
                    }}))
                }).expect(1).mount(&server).await;
            let issuer = environment.issuer.clone();
            Mock::given(method("POST")).and(path("/auth/uc/bootstrap"))
                .and(header("Authorization", "Bearer synthetic-native-token"))
                .respond_with(move |_: &wiremock::Request| {
                    assert!(ready.load(Ordering::SeqCst), "MOI account mapping must precede model policy lookup");
                    if failure == Some("astra") { return ResponseTemplate::new(503); }
                    ResponseTemplate::new(200).set_body_json(json!({"issuer":issuer, "subject":"user", "user_id":"astra-user",
                        "session_id": if failure == Some("session") {"different-session"} else {"session"}}))
                }).expect(if matches!(failure, Some("moi" | "identity")) {0} else {1}).mount(&server).await;
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let result =
                prepare_products(&client, &environment, "synthetic-native-token", "user").await;
            if failure.is_none() {
                let (_, moi) = result.unwrap();
                assert!(moi.workspaces.is_empty());
                assert_eq!(moi.workspace_initialization.status, "pending");
            } else {
                assert!(result.is_err(), "accepted {failure:?}");
            }
        }
    }

    #[tokio::test]
    async fn explicit_profile_keeps_memoria_and_password_discovery() {
        use serde_json::json;
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
        for memoria in [None, Some("https://memoria.example.test")] {
            let server = MockServer::start().await;
            Mock::given(path("/auth/methods"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "password": true,
                    "memoria": memoria.map(|url| json!({"issuer": url, "authorization_url": url})),
                    // UC configuration cannot redirect or break legacy login.
                    "uc": {"issuer": "incomplete"}
                })))
                .expect(1)
                .mount(&server)
                .await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let method = super::super::discover_login_method(&api, Some("personal"))
                .await
                .unwrap();
            match (method, memoria) {
                (super::super::LoginMethod::Memoria(actual), Some(expected)) => {
                    assert_eq!(actual, expected)
                }
                (super::super::LoginMethod::Password, None) => (),
                _ => panic!("explicit legacy profile selected the wrong provider"),
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unprofiled_login_discovers_uc_and_preserves_legacy_servers() {
        let _home = crate::test_utils::HomeGuard::temp();
        let _native_directory = crate::test_utils::ProcessEnvGuard::remove("MOI_AUTH_DIR");
        use serde_json::json;
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
        for (status, body, expected) in [
            (
                200,
                json!({"uc": {
                    "issuer": "https://uc.example.test/realms/moi", "client_id": "astra-cli",
                    "moi_api_url": "https://moi.example.test/newmoi",
                    "scope": "openid profile email astra:user aistudio:user", "resource": "astra-api"
                }}),
                Ok(true),
            ),
            (200, json!({"password": true}), Ok(false)),
            (404, json!({}), Ok(false)),
            (503, json!({}), Err(())),
            (200, json!({"uc": {"issuer": "incomplete"}}), Err(())),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/auth/methods"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            assert_eq!(
                super::super::discover_login_method(&api, None)
                    .await
                    .map(|v| matches!(v, super::super::LoginMethod::Uc(_)))
                    .map_err(|_| ()),
                expected
            );
        }
    }

    #[test]
    fn callback_rejects_mixed_duplicate_and_foreign_identity() {
        assert_eq!(
            callback_code("GET", "/callback?code=c&state=s", &[], "s", "issuer").unwrap(),
            Callback::Code("c".into())
        );
        assert_eq!(
            callback_code(
                "GET",
                "/callback?error=access_denied&state=s",
                &[],
                "s",
                "issuer"
            )
            .unwrap(),
            Callback::Denied
        );
        for target in [
            "/callback?code=c&state=other",
            "/callback?code=c&state=s&state=s",
            "/callback?code=c&state=s&iss=other",
            "/callback?code=c&state=s&code=d",
            "/callback?error=access_denied&state=other",
            "/callback?error=denied&code=c&state=s",
            "/callback?code=c&state=s#fragment",
            "/other?code=c&state=s",
            "/callback?code=c&state=s&token=secret",
        ] {
            assert!(
                callback_code("GET", target, &[], "s", "issuer").is_err(),
                "accepted {target}"
            );
        }
        assert!(callback_code("POST", "/callback?code=c&state=s", &[], "s", "issuer").is_err());
        assert!(callback_code("GET", "/callback?code=c&state=s", b"body", "s", "issuer").is_err());
    }

    #[test]
    fn terminal_projection_strips_escape_controls() {
        assert!(!terminal_text("name\u{1b}[2J\n").contains('\u{1b}'));
        assert_eq!(terminal_text(&"x".repeat(4096)).len(), 1024);
    }
}

async fn verify_id_token(
    client: &reqwest::Client,
    environment: &Environment,
    token: &str,
    nonce: &str,
) -> Result<String, String> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
    let header = decode_header(token).map_err(|_| "invalid UC ID token header")?;
    if header.alg != Algorithm::RS256 {
        return Err("unsupported UC ID token algorithm".into());
    }
    let kid = header.kid.ok_or("missing UC signing key ID")?;
    let response = client
        .get(&environment.jwks_uri)
        .send()
        .await
        .map_err(|_| "UC signing keys unavailable")?;
    if !response.status().is_success() {
        return Err("UC signing key request failed".into());
    }
    let keys: JwkSet = native::bounded_json(response).await?;
    let key = DecodingKey::from_jwk(keys.find(&kid).ok_or("unknown UC signing key")?)
        .map_err(|_| "invalid UC signing key")?;
    #[derive(Clone, Deserialize)]
    struct Claims {
        sub: String,
        nonce: String,
        iat: i64,
        azp: Option<String>,
        aud: serde_json::Value,
    }
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[&environment.issuer]);
    validation.set_audience(&["astra-cli"]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
    validation.validate_nbf = true;
    validation.leeway = 10;
    let claims = decode::<Claims>(token, &key, &validation)
        .map_err(|_| "UC ID token verification failed")?
        .claims;
    let exact_audience = claims.aud == serde_json::json!("astra-cli")
        || claims.aud == serde_json::json!(["astra-cli"]);
    if claims.sub.is_empty()
        || claims.sub.len() > 128
        || claims.iat <= 0
        || claims.iat > native::unix_now()? + 10
        || !exact_audience
        || claims.azp.as_deref().is_some_and(|v| v != "astra-cli")
        || !super::constant_time_eq(claims.nonce.as_bytes(), nonce.as_bytes())
    {
        return Err("UC ID token identity/nonce mismatch".into());
    }
    Ok(claims.sub)
}
