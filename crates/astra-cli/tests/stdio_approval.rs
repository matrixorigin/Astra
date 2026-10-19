//! Public stdio approval transport, using a loopback protocol fixture only.
use astra_cli::cli::mock_llm::{MockLlmServer, MockScenario};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};

fn isolated_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    astra_credentials::CredentialStore::with_path(home.path().join(".astra/credentials.json"))
        .mutate(|credentials| {
            credentials.profiles.insert(
                "stdio-e2e".into(),
                astra_credentials::Profile {
                    account_id: Some("stdio-fixture-owner".into()),
                    access_token: Some("stdio-e2e-token".into()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    home
}

async fn start_mock() -> MockLlmServer {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    let routes = Router::new()
        .route("/agents/edge", post(|| async { Json(json!({"ok": true})) }))
        .route(
            "/skills",
            get(|| async {
                Json(json!({
                    "skills": [], "total": 0, "limit": 100, "next_cursor": null
                }))
            }),
        );
    MockLlmServer::start_with_routes(MockScenario::ToolThenComplete, routes)
        .await
        .unwrap()
}

async fn send(stdin: &mut tokio::process::ChildStdin, id: u64, method: &str, params: Value) {
    let mut request = serde_json::to_vec(&json!({
        "jsonrpc": "2.0", "id": id, "method": method, "params": params
    }))
    .unwrap();
    request.push(b'\n');
    stdin.write_all(&request).await.unwrap();
    stdin.flush().await.unwrap();
}

async fn until(
    lines: &mut Lines<BufReader<tokio::process::ChildStdout>>,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("stdio server output");
            let event: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("non-protocol stdout: {line}: {error}"));
            assert!(event.get("error").is_none(), "RPC failed: {event}");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("stdio protocol transition must finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_approval_over_piped_stdio_can_approve_deny_and_interrupt() {
    for action in ["allow_once", "deny", "interrupt"] {
        let mock = start_mock().await;
        let home = isolated_home();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"))
            .args([
                "--api-url",
                &mock.base_url,
                "--profile",
                "stdio-e2e",
                "--model",
                "mock-model",
                "--no-instructions",
                "serve",
                "stdio",
            ])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("XDG_CACHE_HOME", home.path().join(".cache"))
            .env("XDG_DATA_HOME", home.path().join(".local/share"))
            .env("ASTRA_ACCESS_TOKEN", "stdio-e2e-token")
            .env("ASTRA_API_URL", &mock.base_url)
            .env("ASTRA_CLI_CREDENTIALS_DIR", home.path().join(".astra"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Drain diagnostic output so it cannot backpressure the protocol.
        let mut stderr = child.stderr.take().unwrap();
        let diagnostics = tokio::spawn(async move {
            let mut bytes = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut bytes)
                .await
                .unwrap();
            bytes
        });
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        send(&mut stdin, 1, "initialize", json!({})).await;
        until(&mut lines, |e| e["id"] == 1).await;
        send(&mut stdin, 2, "thread/start", json!({})).await;
        let thread = until(&mut lines, |e| e["id"] == 2).await;
        let thread_id = thread["result"]["thread"]["id"].as_str().unwrap();
        send(
            &mut stdin,
            3,
            "turn/start",
            json!({
                "threadId": thread_id, "input": [{"type": "text", "text": "write the requested fixture file"}],
                "permissionMode": "prompt"
            }),
        )
        .await;
        let turn = until(&mut lines, |e| e["id"] == 3).await;
        let turn_id = turn["result"]["turn"]["id"].as_str().unwrap();
        let approval = until(&mut lines, |e| e["method"] == "approval/requested").await;
        let approval = &approval["params"]["approval"];
        assert_eq!(approval["tool"], "write_file");
        assert_eq!(approval["threadId"], thread_id);
        assert_eq!(approval["turnId"], turn_id);
        assert!(!home.path().join("mock-output-astra-cli.txt").exists());
        let requests = mock.received_requests();
        assert_eq!(requests.len(), 1);
        // This is the actual admission field consumed by Server. A fixture
        // tool request alone would miss the original NonInteractive defect.
        assert_eq!(requests[0]["interaction_mode"], "prompt");
        assert!(
            requests[0]["context"]["edge_profile"]
                [astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RESTRICTED_TOOL_NAMES]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "ask_user"),
            "Server must not reintroduce its own question-tool schema"
        );
        assert!(
            requests[0]["context"]["edge_tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["function"]["name"] != "ask_user")
        );
        let approval_id = approval["id"].as_str().unwrap();
        if action == "interrupt" {
            send(
                &mut stdin,
                4,
                "turn/interrupt",
                json!({
                    "threadId": thread_id, "turnId": turn_id
                }),
            )
            .await;
        } else {
            send(
                &mut stdin,
                4,
                "approval/respond",
                json!({
                    "approvalId": approval_id, "decision": action
                }),
            )
            .await;
        }
        // RPC responses and turn notifications are independent output lanes.
        // Observe both without requiring one to win the completion race.
        let mut acknowledged = false;
        let mut completion = None;
        while !acknowledged || completion.is_none() {
            let event = until(&mut lines, |e| {
                e["id"] == 4 || e["method"] == "turn/completed"
            })
            .await;
            acknowledged |= event["id"] == 4;
            if event["method"] == "turn/completed" {
                completion = Some(event);
            }
        }
        let completion = completion.unwrap();
        assert_eq!(completion["params"]["turn"]["id"], turn_id);
        if action == "allow_once" {
            assert_eq!(completion["params"]["status"], "completed", "{completion}");
            assert_eq!(
                std::fs::read_to_string(home.path().join("mock-output-astra-cli.txt")).unwrap(),
                "Output from astra-cli\n"
            );
            assert_eq!(mock.tool_results()[0]["status"], "completed");
        } else {
            assert!(!home.path().join("mock-output-astra-cli.txt").exists());
            assert!(
                mock.tool_results()
                    .iter()
                    .all(|r| r["status"] != "completed")
            );
            if action == "deny" {
                assert_eq!(mock.tool_results().len(), 1, "denial must reach Server");
            } else {
                assert_eq!(mock.cancelled_runs(), ["mock-run-tool"]);
            }
        }
        // Settled or interrupted requests cannot authorize a later tool call.
        send(
            &mut stdin,
            5,
            "approval/respond",
            json!({
                "approvalId": approval_id, "decision": "allow_once"
            }),
        )
        .await;
        let stale = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = lines.next_line().await.unwrap().unwrap();
                let event: Value = serde_json::from_str(&line).unwrap();
                if event["id"] == 5 {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(stale["error"]["message"], "unknown approvalId");
        drop(stdin);
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        let diagnostics = diagnostics.await.unwrap();
        assert!(
            status.success(),
            "{}",
            String::from_utf8_lossy(&diagnostics)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn piped_prompt_without_approval_transport_fails_before_server_admission() {
    let mock = start_mock().await;
    let home = isolated_home();
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"))
            .args([
                "--api-url",
                &mock.base_url,
                "--profile",
                "stdio-e2e",
                "--model",
                "mock-model",
                "--no-instructions",
                "chat",
                "--permission-mode",
                "prompt",
                "--message",
                "write the fixture file",
            ])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("XDG_CACHE_HOME", home.path().join(".cache"))
            .env("XDG_DATA_HOME", home.path().join(".local/share"))
            .env("ASTRA_ACCESS_TOKEN", "stdio-e2e-token")
            .env("ASTRA_API_URL", &mock.base_url)
            .env("ASTRA_CLI_CREDENTIALS_DIR", home.path().join(".astra"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("unsupported prompt configuration must fail promptly")
    .unwrap();
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostics.contains("requires an approval channel"),
        "{diagnostics}"
    );
    assert!(
        mock.received_requests().is_empty(),
        "no model admission for an unsupported prompt sink"
    );
    assert!(!home.path().join("mock-output-astra-cli.txt").exists());
}
