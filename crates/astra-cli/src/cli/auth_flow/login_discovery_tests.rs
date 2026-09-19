use super::*;
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

// Login completion rebuilds the production Skill/MCP pipeline, whose root
// initialization uses block_in_place on the CLI's multi-thread runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn browser_login_completion_requires_registered_checkout_before_runtime_ready() {
    let _credentials = crate::tests::isolate_credentials();
    let (_sessions, _journal) = crate::tests::isolated_sessions_dir();
    let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
        "default",
        Some("browser-user"),
    )
    .unwrap();
    let token = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjQxMDAwMDAwMDB9.sig";
    save_profile_auth_tokens(
        None,
        "browser-user",
        &AuthTokenPayload {
            user_id: "browser-user".into(),
            access_token: token.into(),
            refresh_token: "refresh".into(),
        },
    )
    .unwrap();

    for status in [503, 200] {
        let server = MockServer::start().await;
        Mock::given(path("/agents/edge"))
            .and(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::header(
                "authorization",
                format!("Bearer {token}"),
            ))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = SessionState::default();
        let result = finish_browser_session_login(&api, None, false, &mut state).await;
        if status == 503 {
            assert!(
                result
                    .err()
                    .unwrap()
                    .contains("local execution registration failed")
            );
            assert!(state.delegation_engine.is_none());
            assert!(state.agent_spawner.is_none());
        } else {
            assert!(result.is_ok());
            assert!(state.agent_spawner.is_some());
            let requests = server.received_requests().await.unwrap();
            let registered = requests
                .iter()
                .find(|r| r.url.path() == "/agents/edge")
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&registered.body).unwrap();
            assert!(!body["materialization_id"].as_str().unwrap().is_empty());
            assert!(!body["worktree_path"].as_str().unwrap().is_empty());
            retire_auth_runtime(&mut state).await;
        }
    }
}

fn uc() -> serde_json::Value {
    json!({"issuer":"https://uc.example.test/realms/moi", "client_id":"astra-cli",
        "moi_api_url":"https://moi.example.test/newmoi",
        "scope":"openid profile email astra:user aistudio:user", "resource":"astra-api"})
}

#[tokio::test]
#[serial_test::serial]
async fn login_discovery_routes_uc_memoria_password_and_legacy_profiles() {
    let _home = crate::test_utils::HomeGuard::temp();
    let _native_directory = crate::test_utils::ProcessEnvGuard::remove("MOI_AUTH_DIR");
    let memoria = json!({"issuer":"https://memory.example.test", "authorization_url":"https://memory.example.test"});
    for (status, body, profile, expected) in [
        (
            200,
            json!({"uc":uc(), "memoria":memoria, "password":true}),
            None,
            "uc",
        ),
        (
            200,
            json!({"uc":uc(), "memoria":memoria, "password":true}),
            Some("self-hosted"),
            "memoria",
        ),
        (
            200,
            json!({"memoria":memoria, "password":false}),
            None,
            "memoria",
        ),
        (200, json!({"password":true}), None, "password"),
        (
            200,
            json!({"uc":{"invalid":true}, "password":true}),
            Some("self-hosted"),
            "password",
        ),
        (404, json!({}), None, "password"),
        (
            200,
            json!({"uc":{"invalid":true}, "password":true}),
            None,
            "error",
        ),
        (200, json!({"password":false}), None, "error"),
        (200, json!({"password":"true"}), None, "error"),
        (
            200,
            json!({"memoria":{"issuer":"issuer", "authorization_url":"http://unsafe.example"},"password":true}),
            None,
            "error",
        ),
        (401, json!({}), None, "error"),
        (503, json!({}), None, "error"),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/auth/methods"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let actual = match discover_login_method(&api, profile).await {
            Ok(LoginMethod::Uc(_)) => "uc",
            Ok(LoginMethod::Memoria(_)) => "memoria",
            Ok(LoginMethod::Password) => "password",
            Err(_) => "error",
        };
        assert_eq!(expected, actual, "status={status}, profile={profile:?}");
    }
}

#[tokio::test]
async fn discovery_transport_and_malformed_payload_do_not_downgrade_to_password() {
    let server = MockServer::start().await;
    Mock::given(path("/auth/methods"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>proxy error</html>"))
        .mount(&server)
        .await;
    let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
    assert!(discover_login_method(&api, None).await.is_err());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let api = astra_thin_client::ThinClient::new(&origin, None).unwrap();
    assert!(discover_login_method(&api, None).await.is_err());
}

#[tokio::test]
async fn cancelled_memoria_browser_does_not_exchange_credentials() {
    let server = MockServer::start().await;
    let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
    let result = browser_login(
        &api,
        None,
        LoginMethod::Memoria(server.uri()),
        std::sync::Arc::new(|_| Err("cancelled".into())),
        false,
    )
    .await;
    assert_eq!(result.unwrap_err(), "cancelled");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn uc_browser_denial_returns_to_tui_without_token_exchange() {
    let server = MockServer::start().await;
    let origin = server.uri();
    Mock::given(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer":origin, "authorization_endpoint":format!("{origin}/authorize"),
            "token_endpoint":format!("{origin}/protocol/openid-connect/token"), "revocation_endpoint":format!("{origin}/protocol/openid-connect/revoke"),
            "jwks_uri":format!("{origin}/protocol/openid-connect/certs")
        }))).expect(1).mount(&server).await;
    Mock::given(path("/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let api = astra_thin_client::ThinClient::new(&origin, None).unwrap();
    let mut discovery: astra_services::auth::uc::UcDiscovery =
        serde_json::from_value(uc()).unwrap();
    discovery.issuer = origin;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let response_tx = std::sync::Mutex::new(Some(response_tx));
    let observer: LoginObserver = std::sync::Arc::new(move |progress| {
        match progress {
            LoginProgress::OpenBrowser(url) => {
                let url = url::Url::parse(&url).unwrap();
                let params: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                let mut callback = url::Url::parse(&params["redirect_uri"]).unwrap();
                callback
                    .query_pairs_mut()
                    .append_pair("state", &params["state"])
                    .append_pair("iss", &url.origin().ascii_serialization())
                    .append_pair("error", "access_denied");
                let response_tx = response_tx.lock().unwrap().take().unwrap();
                tokio::spawn(async move {
                    let response = reqwest::Client::builder()
                        .no_proxy()
                        .build()
                        .unwrap()
                        .get(callback)
                        .send()
                        .await
                        .unwrap();
                    let content_type = response.headers()["content-type"]
                        .to_str()
                        .unwrap()
                        .to_owned();
                    response_tx
                        .send((content_type, response.text().await.unwrap()))
                        .unwrap();
                });
            }
            LoginProgress::Completing => panic!("denial must never exchange credentials"),
        }
        Ok(())
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        browser_login(&api, None, LoginMethod::Uc(discovery), observer, false),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().contains("denied or cancelled"));
    let (content_type, body) = response_rx.await.unwrap();
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert!(body.contains("Couldn’t complete sign-in"));
    assert!(!body.contains("access_denied"));
}
