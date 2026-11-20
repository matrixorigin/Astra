//! Native authority selection and legacy compatibility at the binary boundary.
use std::process::Command;

#[tokio::test]
#[cfg(unix)]
async fn status_distinguishes_pending_owner_without_refresh_or_credential_disclosure() {
    use astra_credentials::native::{Environment, NativeSession, NativeStore};
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let root = tempfile::tempdir().unwrap();
    let store = NativeStore::with_directory(root.path().join(".moi"));
    let issuer = "https://uc.example.test/realms/moi";
    let (session, _) = store
        .publish(NativeSession {
            environment: Environment {
                issuer: issuer.into(),
                astra_url: "https://astra.example.test".into(),
                moi_url: "https://moi.example.test".into(),
                authorization_endpoint: format!("{issuer}/protocol/openid-connect/auth"),
                token_endpoint: format!("{issuer}/protocol/openid-connect/token"),
                revocation_endpoint: format!("{issuer}/protocol/openid-connect/revoke"),
                jwks_uri: format!("{issuer}/protocol/openid-connect/certs"),
            },
            generation: String::new(),
            subject: "user".into(),
            session_id: "session".into(),
            astra_user_id: "astra-user".into(),
            moi_principal_id: "moi-user".into(),
            catalog_user_id: "catalog-user".into(),
            access_token: "private-access".into(),
            refresh_token: "private-refresh".into(),
            expires_at: 0,
            workspace_id: None,
            role_id: None,
            refresh_pending: false,
        })
        .unwrap();
    let path = root.path().join(".moi/auth.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["sessions"][session.environment.key()]["refresh_pending"] = true.into();
    let original = serde_json::to_vec(&state).unwrap();
    std::fs::write(&path, &original).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(
            root.path()
                .join(format!(".moi/refresh-{}.lock", session.environment.key())),
        )
        .unwrap();
    lock.lock_exclusive().unwrap();
    for expected in ["refresh_in_progress", "reauthentication_required"] {
        if expected == "reauthentication_required" {
            FileExt::unlock(&lock).unwrap();
        }
        let output =
            client_output(isolated_client(root.path()).args(["auth", "status", "--json"])).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["state"], expected);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let public = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!public.contains("private-access") && !public.contains("private-refresh"));
    }
}

#[cfg(unix)]
fn isolated_client(root: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"));
    command
        .env_clear()
        .env("HOME", root)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

#[cfg(unix)]
async fn client_output(command: &mut tokio::process::Command) -> std::process::Output {
    tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
        .await
        .expect("local login must complete without browser/interactive input")
        .unwrap()
}

#[tokio::test]
#[cfg(unix)]
async fn unused_moi_directory_does_not_block_legacy_login() {
    use std::os::unix::fs::PermissionsExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    let root = tempfile::tempdir().unwrap();
    let auth = root.path().join(".moi");
    std::fs::create_dir(&auth).unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token":"synthetic-access", "refresh_token":"synthetic-refresh", "user_id":"local-user"
        })))
        .expect(1)
        .mount(&server).await;
    let output = client_output(
        isolated_client(root.path())
            .env("ASTRA_API_URL", server.uri())
            .args([
                "login",
                "--username",
                "local-user",
                "--password",
                "synthetic-password",
            ]),
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_dir(&auth).unwrap().count(), 0);
    assert_eq!(
        std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[tokio::test]
#[cfg(unix)]
async fn fresh_process_login_cannot_downgrade_a_selected_uc_environment() {
    use astra_credentials::native::{Environment, NativeSession, NativeStore};
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    for (status, body) in [
        (404, json!({})),
        (200, json!({"password":true, "uc":null, "memoria":null})),
        (
            200,
            json!({"password":true, "memoria":{
                "issuer":"https://memory.example.test", "authorization_url":"https://memory.example.test"
            }}),
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/methods"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body.clone()))
            .mount(&server)
            .await;
        for logged_out in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = NativeStore::with_directory(root.path().join(".moi"));
            let issuer = "https://uc.example.test/realms/moi";
            store
                .publish(NativeSession {
                    environment: Environment {
                        issuer: issuer.into(),
                        astra_url: server.uri(),
                        moi_url: "https://moi.example.test/newmoi".into(),
                        authorization_endpoint: format!("{issuer}/protocol/openid-connect/auth"),
                        token_endpoint: format!("{issuer}/protocol/openid-connect/token"),
                        revocation_endpoint: format!("{issuer}/protocol/openid-connect/revoke"),
                        jwks_uri: format!("{issuer}/protocol/openid-connect/certs"),
                    },
                    generation: String::new(),
                    subject: "user".into(),
                    session_id: "session".into(),
                    astra_user_id: "astra-user".into(),
                    moi_principal_id: "moi-user".into(),
                    catalog_user_id: "catalog-user".into(),
                    access_token: "synthetic-access".into(),
                    refresh_token: "synthetic-refresh".into(),
                    expires_at: 0,
                    workspace_id: None,
                    role_id: None,
                    refresh_pending: false,
                })
                .unwrap();
            if logged_out {
                store.logout().unwrap();
            }
            let auth_path = root.path().join(".moi/auth.json");
            let before = std::fs::read(&auth_path).unwrap();
            let output = client_output(isolated_client(root.path()).arg("login")).await;
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success());
            assert!(
                error.contains("requires UC login"),
                "status={status}, logged_out={logged_out}: {error}"
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("Username"));
            assert_eq!(before, std::fs::read(&auth_path).unwrap());

            // The explicit legacy escape hatch still reaches password input,
            // without consuming or modifying the selected MOI session.
            if body.get("memoria").is_none_or(serde_json::Value::is_null) {
                let output = client_output(
                    isolated_client(root.path())
                        .env("ASTRA_API_URL", server.uri())
                        .args(["--profile", "personal", "login"]),
                )
                .await;
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("Username cannot be empty")
                );
                assert_eq!(before, std::fs::read(&auth_path).unwrap());
            }
        }
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.url.path() == "/auth/methods")
        );
    }
}

#[test]
fn normal_commands_reject_workspace_auth_directory_but_keep_shell_override() {
    let root = tempfile::tempdir().unwrap();
    let trusted = root.path().join("trusted");
    let untrusted = root.path().join("untrusted");
    std::fs::write(
        root.path().join(".env"),
        format!("MOI_AUTH_DIR={}\n", untrusted.display()),
    )
    .unwrap();
    for shell_override in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_astra"));
        command
            .env_clear()
            .env("HOME", root.path())
            .current_dir(root.path());
        // An invalid local setting makes this deterministic and offline even
        // when the trusted shell override passes the dotenv boundary.
        command.args(["--settings", "{invalid"]);
        if shell_override {
            command.env("MOI_AUTH_DIR", &trusted);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            diagnostic.contains("MOI_AUTH_DIR"),
            !shell_override,
            "{diagnostic}"
        );
    }
    assert!(!untrusted.exists(), "workspace auth store was accessed");
    let output = Command::new(env!("CARGO_BIN_EXE_astra"))
        .env_clear()
        .env("HOME", root.path())
        .env("MOI_AUTH_DIR", &trusted)
        .current_dir(root.path())
        .args(["auth", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("not_configured"));
    assert!(!untrusted.exists());
}

#[tokio::test]
#[cfg(unix)]
async fn broken_device_host_only_affects_device_selection() {
    use astra_credentials::{
        LocalChatgptModel, LocalModelScope,
        chatgpt::{Registration, Store, Tokens},
        native::{Environment, NativeSession, NativeStore},
    };
    use std::os::unix::fs::PermissionsExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    let root = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let origin = server.uri();
    let issuer = format!("{origin}/realms/moi");
    let store = NativeStore::with_directory(root.path().join("auth"));
    let session = store
        .publish(NativeSession {
            environment: Environment {
                issuer: issuer.clone(),
                astra_url: origin.clone(),
                moi_url: format!("{origin}/newmoi"),
                authorization_endpoint: format!("{issuer}/protocol/openid-connect/auth"),
                token_endpoint: format!("{issuer}/protocol/openid-connect/token"),
                revocation_endpoint: format!("{issuer}/protocol/openid-connect/revoke"),
                jwks_uri: format!("{issuer}/protocol/openid-connect/certs"),
            },
            generation: String::new(),
            subject: "fixture-subject".into(),
            session_id: "fixture-session".into(),
            astra_user_id: "fixture-user".into(),
            moi_principal_id: "fixture-moi-user".into(),
            catalog_user_id: "fixture-catalog-user".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            expires_at: astra_credentials::native::unix_now().unwrap() + 3600,
            workspace_id: Some("fixture-workspace".into()),
            role_id: None,
            refresh_pending: false,
        })
        .unwrap()
        .0;
    store
        .select_workspace(&session, Some("fixture-workspace"))
        .unwrap();
    let session = store.current().unwrap();
    let credentials = root.path().join("credentials");
    let scope = {
        let _guard = astra_credentials::set_test_credentials_dir(credentials.clone());
        LocalModelScope::for_native_session(&session).unwrap()
    };
    let chatgpt = Store::new(&scope);
    let connection = uuid::Uuid::new_v4();
    chatgpt
        .authorize(
            connection,
            None,
            "Fixture".into(),
            Registration {
                client_id: "synthetic-client".into(),
                subject: "synthetic-subject".into(),
                host_id: chatgpt.host_id().unwrap(),
            },
            Tokens::new(
                "synthetic-provider-access".into(),
                "synthetic-provider-refresh".into(),
                astra_credentials::native::unix_now().unwrap() + 3600,
            )
            .unwrap(),
            true,
        )
        .unwrap();
    let models = scope.models();
    let mut config = models.load().unwrap();
    config.chatgpt_models.insert(
        "device-model".into(),
        LocalChatgptModel {
            connection_ref: connection,
            authorization_revision: 1,
            model: "fixture".into(),
            binding_revision: 1,
            context_window: 1024,
            max_output_tokens: 64,
            supported_reasoning_efforts: vec![],
        },
    );
    models.replace(config.revision, config).unwrap();
    store
        .enable_device_connection(&session, connection, 1)
        .unwrap();

    let companion = root.path().join("broken-edge");
    // The isolated child records that it ran, then exits before serving IPC.
    std::fs::write(&companion, "#!/bin/sh\n: > edge-started\nexit 17\n").unwrap();
    std::fs::set_permissions(&companion, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cli = || {
        let mut c = isolated_client(root.path());
        c.env("MOI_AUTH_DIR", store.directory())
            .env("ASTRA_CLI_CREDENTIALS_DIR", &credentials)
            .env("ASTRA_EDGE_BIN", &companion)
            .env("ASTRA_EDGE_REGISTRY", "off")
            // Disconnect still clears local credentials, without contacting a
            // real provider during this test.
            .env("ASTRA_RUNNER_CA_BUNDLE", root.path().join("absent-ca"));
        c
    };

    Mock::given(method("GET"))
        .and(path("/skills"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "skills": [], "limit": 100, "total": 0, "next_cursor": null
        })))
        .mount(&server)
        .await;
    let receipt = astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt::new(
        "org-run",
        1,
        0,
        0,
        0,
        astra_turn_core::tool_ledger_receipt::ToolLedgerResultClassCounts::default(),
        0,
        astra_turn_core::tool_ledger_receipt::EMPTY_TOOL_LEDGER_ROOT,
        true,
    );
    let events = [
        serde_json::json!({"type":"session_info","session_id":"org-session","run_id":"org-run"}),
        serde_json::json!({"type":"text_delta","content":"Organization answer"}),
        serde_json::json!({"type":"text_done","full_text":"Organization answer"}),
        serde_json::json!({"type":"usage","input_tokens":1,"output_tokens":2}),
        serde_json::json!({"type":"run_finished","run_id":"org-run","status":"completed","owner_generation":1}),
        serde_json::json!({"type":"turn_complete","has_tool_calls":false,"continuation_owner":"server","tool_calls_count":0,"observation_tool_calls_count":0,"tools_used":[],"llm_rounds":1,"tool_ledger_receipt":receipt}),
    ];
    let stream = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(stream)
                .insert_header("content-type", "text/event-stream")
                .insert_header(
                    astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
                    astra_server_types::AGENT_INTERACTION_API_MAJOR,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [{
                "offering_id": "org-fixture",
                "access_id": "organization",
                "access_kind": "astra_cloud",
                "access_label": "Organization",
                "execution_placement": "server",
                "name": "organization-model",
                "provider": "openai",
                "is_active": true,
                "context_window": 1024
            }],
            "next_cursor": null, "limit": 50, "total": 1,
            "catalog_revision": "sha256:organization"
        })))
        .expect(1..)
        .mount(&server)
        .await;
    let output = client_output(cli().args([
        "chat",
        "--no-resume",
        "--permission-mode",
        "deny",
        "--model",
        "organization-model",
        "-m",
        "fixture",
    ]))
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Organization answer"));
    assert!(!root.path().join("edge-started").exists());

    let output = client_output(cli().args([
        "chat",
        "--no-resume",
        "--permission-mode",
        "deny",
        "--model",
        "runner-fixture",
        "-m",
        "fixture",
    ]))
    .await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("exited before becoming ready"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let started = root.path().join("edge-started");
    assert!(started.is_file());
    std::fs::remove_file(&started).unwrap();

    let output = client_output(cli().args(["model", "disconnect", &connection.to_string()])).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("disconnected_locally"));
    assert!(!started.exists());
    assert!(chatgpt.token_snapshot(connection).is_err());
}
