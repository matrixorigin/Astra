#[cfg(unix)]
#[test]
fn completion_stops_quietly_when_the_consumer_closes_stdout() {
    assert_public_stdout_closes_quietly(&["completion", "zsh"]);
}

#[cfg(unix)]
#[test]
fn help_stops_quietly_when_the_consumer_closes_stdout() {
    assert_public_stdout_closes_quietly(&["--help"]);
}

#[cfg(unix)]
#[test]
fn config_list_stops_quietly_when_the_consumer_closes_stdout() {
    assert_public_stdout_closes_quietly(&["config", "list"]);
}

#[cfg(unix)]
#[test]
fn closed_stdout_state_is_process_local() {
    use std::process::{Command, Stdio};

    let open_consumer = Command::new(env!("CARGO_BIN_EXE_astra"))
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn independent Astra process");
    assert_public_stdout_closes_quietly(&["completion", "zsh"]);
    let output = open_consumer
        .wait_with_output()
        .expect("wait for independent Astra process");

    assert!(
        output.status.success(),
        "sibling process must remain healthy"
    );
    assert!(
        !output.stdout.is_empty(),
        "version output must be preserved"
    );
    assert!(output.stderr.is_empty(), "version stderr must remain quiet");
}

#[test]
fn public_stdout_has_no_direct_writer_bypass() {
    fn visit(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read Astra CLI source tree") {
            let path = entry.expect("read source entry").path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }

    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sink_path = source_root.join("cli/stream/output_sink.rs");
    let mut files = Vec::new();
    visit(&source_root, &mut files);
    for path in files {
        if path == sink_path {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read Astra CLI source");
        let compact = source
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        let without_terminal_queries = compact
            .replace("std::io::stdout().is_terminal()", "")
            .replace("io::stdout().is_terminal()", "")
            .replace("std::io::IsTerminal::is_terminal(&std::io::stdout())", "");
        assert!(
            !without_terminal_queries.contains("io::stdout()"),
            "public stdout write bypasses output_sink: {}",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn short_output_without_newline_closes_quietly() {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "short_output_without_newline_probe",
            "--nocapture",
        ])
        .env("ASTRA_SHORT_STDOUT_PROBE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn short-output probe");

    // Keep stdout open while libtest lists and starts the selected test. The
    // probe announces that it owns execution before the parent closes the
    // consumer, making the EPIPE belong to Astra's output sink rather than to
    // libtest's own status rendering.
    let mut stderr = std::io::BufReader::new(child.stderr.take().expect("piped stderr"));
    let mut ready = String::new();
    stderr.read_line(&mut ready).expect("read probe readiness");
    assert_eq!(ready, "ASTRA_SHORT_STDOUT_PROBE_READY\n");
    drop(child.stdout.take().expect("piped stdout"));
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"go")
        .expect("release short-output probe");
    let status = child.wait().expect("wait for short-output probe");
    let mut remaining_stderr = String::new();
    stderr
        .read_to_string(&mut remaining_stderr)
        .expect("read remaining probe stderr");

    assert_eq!(status.signal(), None);
    assert_eq!(status.code(), Some(141));
    assert!(
        !remaining_stderr.contains("panicked") && !remaining_stderr.contains("Broken pipe"),
        "closed stdout must not produce a panic: {remaining_stderr}"
    );
}

#[cfg(unix)]
#[test]
fn short_output_without_newline_probe() {
    if std::env::var_os("ASTRA_SHORT_STDOUT_PROBE").is_none() {
        return;
    }
    astra_cli::cli::stream::output_sink::configure_process_output_signals()
        .expect("set production signal policy");
    eprintln!("ASTRA_SHORT_STDOUT_PROBE_READY");
    let mut release = [0_u8; 1];
    std::io::Read::read_exact(&mut std::io::stdin(), &mut release)
        .expect("wait for parent to close stdout");
    let _ = astra_cli::cli::stream::output_sink::write_stdout_fmt(format_args!("short"));
    let _ = astra_cli::cli::stream::output_sink::flush_stdout();
    std::process::exit(astra_cli::cli::stream::output_sink::resolved_exit_code(0));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_stream_closed_after_run_binding_cancels_exact_server_run() {
    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::Path;
    use axum::http::{Response, header};
    use axum::routing::{delete, get, post};
    use std::convert::Infallible;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncBufReadExt as _;

    let (release_final_text, final_text_release) = tokio::sync::oneshot::channel::<()>();
    let final_text_release = Arc::new(tokio::sync::Mutex::new(Some(final_text_release)));
    let release_for_stream = final_text_release.clone();
    let cancel_count = Arc::new(AtomicUsize::new(0));
    let cancel_count_for_route = cancel_count.clone();
    let model_count = Arc::new(AtomicUsize::new(0));
    let model_count_for_route = model_count.clone();
    let chat_count = Arc::new(AtomicUsize::new(0));
    let chat_count_for_route = chat_count.clone();

    let app = Router::new()
        .route("/agents/edge", post(|| async { axum::Json(serde_json::json!({"ok": true})) }))
        // One-shot pipeline construction always discovers the authenticated
        // remote skill catalog before opening /chat/stream. An empty page is
        // the generic valid contract for a user with no published skills.
        .route(
            "/skills",
            get(|| async {
                axum::Json(serde_json::json!({
                    "skills": [],
                    "total": 0,
                    "limit": 100,
                    "next_cursor": null
                }))
            }),
        )
        .route(
            "/models",
            get(move || {
                let model_count_for_route = model_count_for_route.clone();
                async move {
                    model_count_for_route.fetch_add(1, Ordering::SeqCst);
                    pipe_model_catalog()
                }
            }),
        )
        .route(
            "/chat/stream",
            post(move || {
                let release_for_stream = release_for_stream.clone();
                let chat_count_for_route = chat_count_for_route.clone();
                async move {
                    chat_count_for_route.fetch_add(1, Ordering::SeqCst);
                    let release = release_for_stream
                        .lock()
                        .await
                        .take()
                        .expect("one chat stream");
                    let stream = futures_util::stream::unfold(
                        (0_u8, Some(release)),
                        |(phase, release)| async move {
                            match phase {
                                0 => Some((
                                    Ok::<Bytes, Infallible>(Bytes::from_static(
                                        b"data: {\"type\":\"session_info\",\"session_id\":\"pipe-session\",\"run_id\":\"pipe-run\"}\n\n",
                                    )),
                                    (1, release),
                                )),
                                1 => {
                                    release.expect("final text release").await.ok();
                                    Some((
                                        Ok(Bytes::from_static(
                                            b"data: {\"type\":\"text_delta\",\"content\":\"answer after binding\"}\n\ndata: {\"type\":\"text_done\",\"full_text\":\"answer after binding\"}\n\ndata: {\"type\":\"usage\",\"input_tokens\":1,\"output_tokens\":3}\n\ndata: {\"type\":\"turn_complete\",\"has_tool_calls\":false,\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":1}\n\ndata: [DONE]\n\n",
                                        )),
                                        (2, None),
                                    ))
                                }
                                _ => None,
                            }
                        },
                    );
                    Response::builder()
                        .status(200)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(
                            astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
                            astra_server_types::AGENT_INTERACTION_API_MAJOR,
                        )
                        .body(Body::from_stream(stream))
                        .expect("SSE response")
                }
            }),
        )
        .route(
            "/chat/runs/{run_id}",
            delete(move |Path(run_id): Path<String>| {
                let cancel_count_for_route = cancel_count_for_route.clone();
                async move {
                    assert_eq!(run_id, "pipe-run");
                    cancel_count_for_route.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "run_id": run_id,
                        "status": "cancelled",
                        "execution_settled": true
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Astra server");
    let base_url = format!("http://{}", listener.local_addr().expect("mock address"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock Astra API");
    });

    let home = tempfile::tempdir().expect("isolated Astra home");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"));
    child
        .args([
            "--api-url",
            &base_url,
            "--profile",
            "pipe-e2e",
            "--model",
            "mock-model",
            "--no-instructions",
            "--print",
            "--output-format",
            "stream-json",
            "exercise active output lifecycle",
        ])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .env("ASTRA_ACCESS_TOKEN", "pipe-e2e-token")
        .env("ASTRA_API_URL", &base_url)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = child.spawn().expect("spawn active-stream Astra");
    let mut stdout_lines =
        tokio::io::BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut stdout = String::new();
    loop {
        let line = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            stdout_lines.next_line(),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Astra must bind the streamed run; models={}, chats={}, stdout so far: {stdout}",
                model_count.load(Ordering::SeqCst),
                chat_count.load(Ordering::SeqCst),
            )
        })
        .expect("read Astra stdout");
        let Some(line) = line else {
            use tokio::io::AsyncReadExt;
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .expect("piped stderr")
                .read_to_string(&mut stderr)
                .await
                .expect("read early-exit stderr");
            panic!(
                "Astra exited before binding the streamed run; stdout: {stdout}; stderr: {stderr}"
            );
        };
        stdout.push_str(&line);
        stdout.push('\n');
        let bound = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .is_some_and(|event| {
                event["type"] == "sse_event"
                    && event["event"]["type"] == "session_info"
                    && event["event"]["run_id"] == "pipe-run"
            });
        if bound {
            break;
        }
    }

    // The machine stream's accepted session_info record proves the exact
    // durable owner was captured before the consumer disappears.
    drop(stdout_lines);
    release_final_text
        .send(())
        .expect("release final text after stdout closure");
    let status = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait())
        .await
        .expect("Astra must settle the closed output")
        .expect("wait for active-stream Astra");
    let mut stderr = String::new();
    tokio::io::AsyncReadExt::read_to_string(
        &mut child.stderr.take().expect("piped stderr"),
        &mut stderr,
    )
    .await
    .expect("read final stderr");
    server.abort();

    assert_eq!(status.signal(), None, "closed stdout is not a signal");
    assert_eq!(status.code(), Some(141), "closed stdout is explicit EPIPE");
    assert_eq!(
        cancel_count.load(Ordering::SeqCst),
        1,
        "the exact bound server run must be cancelled and settled once"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("Broken pipe"),
        "active stdout closure must remain panic-free: {stderr}"
    );
}

#[cfg(unix)]
fn assert_public_stdout_closes_quietly(args: &[&str]) {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_astra"))
        .args(args)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn astra command");

    // Close before the child can write. Unlike a real `head -1` process this
    // cannot race with a small producer completing into the kernel pipe
    // buffer, so the child-status assertion is deterministic.
    drop(child.stdout.take().expect("piped stdout"));

    let output = child.wait_with_output().expect("wait for astra completion");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.signal(),
        None,
        "closed stdout is not a signal"
    );
    assert_eq!(
        output.status.code(),
        Some(141),
        "closed stdout has explicit Unix pipeline status: {args:?}"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("Broken pipe"),
        "closed stdout must not produce a panic: {stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn closed_child_stdin_is_an_io_error_not_a_process_signal() {
    use std::os::unix::process::ExitStatusExt;

    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "closed_child_stdin_sigpipe_probe", "--nocapture"])
        .env("ASTRA_CLOSED_CHILD_STDIN_SIGPIPE_PROBE", "1")
        .output()
        .expect("spawn isolated child-stdin probe");
    assert_eq!(
        output.status.signal(),
        None,
        "a recoverable child-stdin EPIPE must not kill Astra: {:?}",
        output.status
    );
    assert!(output.status.success(), "probe must observe BrokenPipe");
}

#[cfg(target_os = "linux")]
#[test]
fn closed_child_stdin_sigpipe_probe() {
    if std::env::var_os("ASTRA_CLOSED_CHILD_STDIN_SIGPIPE_PROBE").is_none() {
        return;
    }
    use std::io::Write as _;
    use std::process::Stdio;

    astra_cli::cli::stream::output_sink::configure_process_output_signals()
        .expect("set production signal policy");
    let mut child = std::process::Command::new("sh")
        .args(["-c", "exit 0"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn early-exit child");
    let mut stdin = child.stdin.take().expect("child stdin");
    child.wait().expect("early-exit child");
    let error = stdin
        .write_all(b"glob candidate\n")
        .expect_err("closed child stdin must report EPIPE");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
}

fn pipe_model_catalog() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
    "items": [{
        "offering_id": "pipe-offering",
        "access_id": "pipe-access",
        "access_kind": "self_hosted",
        "access_label": "Pipe E2E",
        "execution_placement": "server",
        "name": "mock-model",
        "provider": "mock",
        "description": null,
        "is_active": true,
        "context_window": 128000,
        "max_completion_tokens": null,
        "architecture": null,
        "thinking_capability": null
    }],
    "next_cursor": null,
    "limit": 50,
    "total": 1,
    "catalog_revision": "sha256:pipe-e2e"
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_hard_failure_preserves_only_the_admitted_root_identity() {
    use axum::{
        Router,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use std::process::Stdio;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    const SESSION: &str = "395d819b-81a7-4cc0-9aa8-3666ec1ac194";
    for (admitted, closed) in [(true, true), (true, false), (false, false)] {
        let requests = Arc::new(AtomicUsize::new(0));
        let observed_requests = requests.clone();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed_reads = reads.clone();
        let deletes = Arc::new(AtomicUsize::new(0));
        let observed_deletes = deletes.clone();
        let app = Router::new()
            .route(
                "/sessions",
                post(|| async { axum::Json(json!({"session_id": SESSION})) }),
            )
            .route("/agents/edge", post(|| async { axum::Json(json!({"ok": true})) }))
            .route("/skills", get(|| async { axum::Json(json!({
                "skills": [], "total": 0, "limit": 100, "next_cursor": null
            })) }))
            .route("/models", get(|| async { pipe_model_catalog() }))
            .route("/chat/stream", post(move || { let calls = observed_requests.clone(); async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let mut events = Vec::new();
                events.push(if admitted {
                    json!({"type": "session_info", "session_id": SESSION, "run_id": "failure-root"})
                } else {
                    json!({"type": "session_info", "session_id": SESSION})
                });
                // A descendant event is never authority to bind or replace the root.
                events.push(json!({"type": "run_started", "run_id": "failure-child", "parent_run_id": "failure-root"}));
                if admitted {
                    events.push(json!({"type": "text_delta", "content": "Observed partial output"}));
                    events.push(json!({"type": "text_done", "full_text": "Observed partial output", "model_item_id":"partial-model-item", "partial":true}));
                    events.push(json!({"type": "usage", "usage_scope":"run_total", "input_tokens": 10, "output_tokens": 5, "qualified_usage":{"input_tokens":10,"output_tokens":5}}));
                }
                events.push(json!({"type": "error", "message": "provider stream stopped", "error_code": "stream_transport", "error_kind":"stream_transport", "metadata":{"interruption":{"kind":"stream_transport","resumable":true}}}));
                if closed {
                    events.push(json!({"type":"run_error", "run_id":"failure-root", "error":"provider stream stopped", "error_code":"stream_transport"}));
                    events.push(json!({"type": "run_finished", "run_id": "failure-root", "status": "failed", "owner_generation": 1, "error_kind": "stream_transport", "error": "provider stream stopped"}));
                }
                let body = events.into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>() + "data: [DONE]\n\n";
                ([
                    ("content-type", "text/event-stream"),
                    (astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER, astra_server_types::AGENT_INTERACTION_API_MAJOR),
                ], body)
            }}))
            .route("/chat/runs/{run_id}", get(move || { let reads=observed_reads.clone(); async move { reads.fetch_add(1, Ordering::SeqCst); axum::Json(json!({
                "status": "failed", "accounting": {
                    "prompt_tokens": 10, "completion_tokens": 5,
                    "cache_read_tokens": 0, "cache_creation_tokens": 0, "tool_call_count": 0
                }
            })) }}).delete(move || { let deletes=observed_deletes.clone(); async move { deletes.fetch_add(1, Ordering::SeqCst); axum::Json(json!({"status":"cancelled"})) }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for mode in ["chat-json", "print-json", "stream-json", "chat-text"] {
            let before = requests.load(Ordering::SeqCst);
            let home = tempfile::tempdir().unwrap();
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"));
            command.args([
                "--api-url",
                &base_url,
                "--profile",
                "pipe-e2e",
                "--model",
                "mock-model",
                "--no-instructions",
            ]);
            if mode == "chat-json" || mode == "chat-text" {
                command.args([
                    "chat",
                    "--no-resume",
                    "--permission-mode",
                    "auto",
                    "-m",
                    "exercise hard failure",
                ]);
                if mode == "chat-json" {
                    command.arg("--json");
                }
            } else {
                command.args([
                    "--print",
                    "--output-format",
                    if mode == "print-json" {
                        "json"
                    } else {
                        "stream-json"
                    },
                    "exercise hard failure",
                ]);
            }
            command
                .current_dir(home.path())
                .env("HOME", home.path())
                .env("ASTRA_LOCAL_STATE_ROOT", home.path().join(".astra"))
                .env("XDG_CONFIG_HOME", home.path().join(".config"))
                .env("XDG_CACHE_HOME", home.path().join(".cache"))
                .env("XDG_DATA_HOME", home.path().join(".local/share"))
                .env("ASTRA_ACCESS_TOKEN", "pipe-e2e-token")
                .env("ASTRA_API_URL", &base_url)
                .env("NO_COLOR", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
                .await
                .unwrap()
                .unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(
                    if !admitted && mode == "stream-json" {
                        "without a bridge run_id"
                    } else {
                        "stream stopped"
                    }
                ),
                "{mode}, admitted={admitted}: stdout={stdout}; stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                output.status.code(),
                Some(3),
                "{mode}, admitted={admitted}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                requests.load(Ordering::SeqCst),
                before + 1,
                "no implicit retry"
            );
            assert_eq!(
                reads.load(Ordering::SeqCst),
                0,
                "no accounting reconciliation"
            );
            assert_eq!(
                deletes.load(Ordering::SeqCst),
                0,
                "a failed terminal is already closed"
            );
            if mode == "chat-text" {
                if admitted {
                    assert_eq!(
                        stdout.matches("Observed partial output").count(),
                        1,
                        "{mode}, admitted={admitted}, closed={closed}: stdout={stdout}; stderr={}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                assert_no_completed_turn(home.path().join(".astra/sessions"), SESSION);
                continue;
            }
            let diagnostic = if !admitted && mode == "stream-json" {
                "without a bridge run_id"
            } else {
                "stream stopped"
            };
            assert_eq!(
                String::from_utf8_lossy(&output.stderr)
                    .matches(diagnostic)
                    .count(),
                1,
                "{mode}: product failure must be printed once"
            );
            let result: Value = if mode == "stream-json" {
                let records = stdout
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>();
                let results = records
                    .iter()
                    .filter(|record| record["type"] == "result")
                    .collect::<Vec<_>>();
                assert_eq!(results.len(), 1);
                assert!(
                    results[0]["result"].get("run_id").is_none(),
                    "Server identity belongs to the exchange, not the CLI execution envelope"
                );
                if admitted {
                    assert!(records.iter().any(|record| record["type"] == "sse_event"
                        && record["event"]["type"] == "session_info"
                        && record["event"]["run_id"] == "failure-root"));
                } else {
                    assert!(
                        records
                            .iter()
                            .all(|record| record["server_run_id"].is_null())
                    );
                }
                results[0]["result"].clone()
            } else {
                let result: Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(
                    result["run_id"],
                    if admitted {
                        json!("failure-root")
                    } else {
                        Value::Null
                    },
                    "{mode}, admitted={admitted}: {stdout}"
                );
                result
            };
            assert_eq!(result["final_state"], "failed");
            assert_eq!(result["exit_code"], 3);
            assert_eq!(result["error_kind"], "api_error");
            assert_eq!(result["success"], false);
            assert_eq!(result["completion_disposition"], "failed");
            assert_eq!(result["server_terminal_unverified"], !closed);
            assert_eq!(result["server_terminal_authoritative"], closed);
            assert_eq!(result["tool_record_coverage"], "partial");
            assert_eq!(result["error_code"], "stream_transport");
            assert!(result["error"].as_str().unwrap().contains(
                if !admitted && mode == "stream-json" {
                    "without a bridge run_id"
                } else {
                    "stream stopped"
                }
            ));
            assert_eq!(
                result["text"],
                if admitted {
                    "Observed partial output"
                } else {
                    ""
                },
                "only actually observed output survives"
            );
            if admitted {
                assert_eq!(result["session_id"], SESSION);
                assert_eq!(result["model_item_id"], "partial-model-item");
                assert_eq!(
                    result["qualified_usage"],
                    if closed {
                        json!({"input_tokens":10,"output_tokens":5})
                    } else {
                        json!({})
                    }
                );
                assert_eq!(result["llm_rounds"], Value::Null);
                assert_eq!(result["fresh_prompt_tokens"], 10);
                assert_eq!(result["completion_tokens"], 5);
                assert_no_completed_turn(home.path().join(".astra/sessions"), SESSION);
            }
        }
        server.abort();
    }
}

fn assert_no_completed_turn(directory: std::path::PathBuf, session_id: &str) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_completed_turn(path, session_id);
        } else if path.file_name().and_then(|name| name.to_str())
            == Some(&format!("{session_id}.jsonl"))
        {
            for line in std::fs::read_to_string(path).unwrap().lines() {
                let event: serde_json::Value = serde_json::from_str(line).unwrap();
                assert_ne!(
                    event["type"], "turn",
                    "a hard failure must not commit a completed turn"
                );
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_sigint_settles_exact_run_without_wall_deadline() {
    assert_one_shot_sigint(true, true, false, false, false, false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_sigint_preserves_wall_deadline_contract() {
    assert_one_shot_sigint(true, true, true, false, false, false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_sigint_unsettled_cancel_is_not_success() {
    assert_one_shot_sigint(false, true, false, false, false, false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_sigint_without_run_identity_never_cancels_session() {
    assert_one_shot_sigint(false, false, false, false, false, false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_second_sigint_returns_unverified_json() {
    assert_one_shot_sigint(false, true, false, true, false, false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_sigint_interrupts_setup_before_stream_admission() {
    assert_one_shot_sigint(false, false, false, false, true, false).await;
}

/// Exercise the real command router and process signal policy with a held
/// response stream. No provider, credential store or model request is involved.
#[cfg(unix)]
async fn assert_one_shot_sigint(
    settled: bool,
    identity: bool,
    wall: bool,
    second: bool,
    setup: bool,
    local_fenced: bool,
) {
    use axum::body::{Body, Bytes};
    use axum::extract::Path;
    use axum::http::{Response, header};
    use axum::routing::{delete, get, post};
    use std::convert::Infallible;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let chats = Arc::new(AtomicUsize::new(0));
    let cancels = Arc::new(AtomicUsize::new(0));
    let chat_counter = chats.clone();
    let cancel_counter = cancels.clone();
    let models = Arc::new(AtomicUsize::new(0));
    let model_counter = models.clone();
    let app = axum::Router::new()
        .route("/agents/edge", post(|| async { axum::Json(serde_json::json!({"ok": true})) }))
        // Hosts with native capacity create the same provisional session
        // before model admission; hosts without it bind from session_info.
        .route("/sessions", post(|| async { axum::Json(serde_json::json!({
            "session_id": "sigint-session"
        })) }))
        .route("/skills", get(|| async { axum::Json(serde_json::json!({
            "skills": [], "total": 0, "limit": 100, "next_cursor": null
        })) }))
        .route("/models", get(move || {
            let models = model_counter.clone();
            async move {
                models.fetch_add(1, Ordering::SeqCst);
                if setup { std::future::pending::<()>().await; }
                pipe_model_catalog()
            }
        }))
        .route("/chat/stream", post(move || {
            let chats = chat_counter.clone();
            async move {
                chats.fetch_add(1, Ordering::SeqCst);
                let stream = futures_util::stream::once(async move {
                    Ok::<Bytes, Infallible>(Bytes::from(if identity {
                        "data: {\"type\":\"session_info\",\"session_id\":\"sigint-session\",\"run_id\":\"sigint-run\"}\n\ndata: {\"type\":\"text_delta\",\"content\":\"observed partial\"}\n\ndata: {\"type\":\"usage\",\"input_tokens\":2,\"output_tokens\":1}\n\ndata: {\"type\":\"warning\",\"message\":\"sigint-partial-ready\"}\n\n"
                    } else {
                        ": held before authoritative admission identity\n\n"
                    }))
                }).chain(futures_util::stream::pending());
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
                        astra_server_types::AGENT_INTERACTION_API_MAJOR)
                    .body(Body::from_stream(stream)).unwrap()
            }
        }))
        .route("/chat/runs/{run_id}", delete(move |Path(run): Path<String>| {
            let cancels = cancel_counter.clone();
            async move {
                assert_eq!(run, "sigint-run", "cancel only the observed exact run");
                cancels.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({"execution_settled": settled}))
            }
        }));
    use futures_util::StreamExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    if local_fenced {
        // Synthetic pre-existing canonical fence; never clear any real workspace guard.
        assert!(
            astra_tools::workspace_observation::mark_workspace_observation_unsettled(home.path())
        );
    }
    let events = home.path().join("events.jsonl");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"));
    command
        .args([
            "--api-url",
            &base,
            "--profile",
            "sigint-test",
            "--model",
            "mock-model",
            "--no-instructions",
            "chat",
            "--json",
            "--no-resume",
            "--permission-mode",
            "bypass",
            "--stream-events",
        ])
        .arg(&events)
        .args(["-m", "held offline signal test"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .env("ASTRA_ACCESS_TOKEN", "synthetic-offline-signal-test")
        .env("ASTRA_API_URL", &base)
        .env("ASTRA_LOCAL_STATE_ROOT", home.path().join("state"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if wall {
        command.args(["--max-wall-time-seconds", "120"]);
    }
    let mut child = command.spawn().unwrap();
    let pid = nix::unistd::Pid::from_raw(child.id().unwrap() as i32);
    let readiness = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let ready = if setup {
                models.load(Ordering::SeqCst) > 0
            } else if identity {
                // Quiet mode defers token projection. The ordered warning
                // receipt proves the preceding partial text was consumed.
                std::fs::read_to_string(&events)
                    .unwrap_or_default()
                    .contains("sigint-partial-ready")
            } else {
                chats.load(Ordering::SeqCst) > 0
            };
            if ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if readiness.is_err() {
        let _ = child.kill().await;
        let output = child.wait_with_output().await.unwrap();
        let excerpt = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes)
                .chars()
                .take(4096)
                .collect::<String>()
        };
        panic!(
            "CLI did not reach held stream; status={}; stdout={}; stderr={}",
            output.status,
            excerpt(&output.stdout),
            excerpt(&output.stderr)
        );
    }
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
    if second {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while cancels.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output())
        .await
        .expect("bounded graceful SIGINT exit")
        .unwrap();
    server.abort();
    assert_eq!(
        output.status.signal(),
        None,
        "SIGINT must produce a terminal JSON document"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "terminal JSON: {error}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(!output.status.success());
    assert_eq!(
        chats.load(Ordering::SeqCst),
        usize::from(!setup),
        "never replay admission"
    );
    if identity {
        assert_eq!(json["run_id"], "sigint-run");
        assert!(cancels.load(Ordering::SeqCst) > 0);
    } else {
        assert_eq!(cancels.load(Ordering::SeqCst), 0);
    }
    if settled && !local_fenced {
        assert_eq!(output.status.code(), Some(5));
        assert_eq!(json["final_state"], "interrupted");
        assert_eq!(json["interruption_kind"], "execution_incomplete");
        assert_eq!(json["interruption_reason"], "user_interrupted");
        assert_eq!(json["text"], "observed partial");
        assert_eq!(json["server_terminal_unverified"], false);
        // Exercise canonical replay of what the real router committed before
        // publishing the interrupted JSON, not just its display envelope.
        let _journal_root = astra_services::session_journal::JournalDirGuard::new(
            home.path().join("state/sessions"),
        );
        let owner_id = astra_credentials::local_profile_owner_id("sigint-test", None).unwrap();
        let owner = astra_services::OwnerScope::user(&owner_id).unwrap();
        let events =
            astra_services::session_journal::read_journal_for_owner(&owner, "sigint-session")
                .unwrap();
        let commits = events
            .into_iter()
            .filter_map(|event| event.conversation_commit);
        let restored = astra_turn_core::active_conversation::ActiveConversation::replay(
            &owner_id,
            "sigint-session",
            commits,
        )
        .unwrap()
        .expect("interrupted partial must have a canonical commit");
        assert_eq!(restored.cursor().completed_turn, 1);
        assert_eq!(
            restored.messages().last().unwrap()["content"],
            "observed partial"
        );
    } else {
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(json["final_state"], "failed");
        assert_eq!(json["server_terminal_unverified"], true);
        assert_eq!(json["success"], false);
        if local_fenced {
            assert_eq!(json["text"], "observed partial");
            assert_eq!(json["persistence_error"], serde_json::Value::Null);
            assert!(
                json["error"]
                    .as_str()
                    .unwrap()
                    .contains("workspace_ownership_unsettled")
            );
            assert!(
                json["error"]
                    .as_str()
                    .unwrap()
                    .contains("local execution settlement is unverified")
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_sigint_server_settled_cannot_hide_local_ownership_fence() {
    assert_one_shot_sigint(true, true, false, false, false, true).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_sigint_exits_while_stdin_writer_remains_open() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;

    let home = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"))
        .args(["--no-instructions", "chat", "--json", "--stdin"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut writer = child.stdin.take().unwrap();
    // More than a pipe buffer proves the child is reading, which follows
    // signal registration. Keep the writer open until after process exit.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        writer.write_all(&vec![b'x'; 2 * 1024 * 1024]),
    )
    .await
    .expect("CLI must start reading stdin")
    .unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id().unwrap() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("held stdin must not block SIGINT or runtime shutdown")
        .unwrap();
    drop(writer);
    assert_eq!(output.status.signal(), None);
    assert_eq!(output.status.code(), Some(3));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["final_state"], "failed");
    assert_eq!(json["server_terminal_unverified"], true);
    assert!(json["run_id"].is_null());
}

/// Reproduce identity failure followed by event-file ENOSPC through the real
/// process entrypoint. Native shutdown timeouts have a separate production-
/// boundary test; this fixture does not claim to stall a real native owner.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_sigint_keeps_identity_failure_when_event_writer_also_fails() {
    use axum::body::{Body, Bytes};
    use axum::extract::Path;
    use axum::http::{Response, header};
    use axum::routing::{delete, get, post};
    use futures_util::StreamExt as _;
    use std::convert::Infallible;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::Stdio;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let fixture = tempfile::tempdir().unwrap();
    let fault = fixture.path().join("stream_event_enospc.so");
    let compiler = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&fault)
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/stream_event_enospc.c"
        ))
        .arg("-ldl")
        .output()
        .expect("compile the isolated event-file fault fixture");
    assert!(
        compiler.status.success(),
        "fault fixture: {}",
        String::from_utf8_lossy(&compiler.stderr)
    );
    for inject_enospc in [false, true] {
        let chats = Arc::new(AtomicUsize::new(0));
        let cancels = Arc::new(AtomicUsize::new(0));
        let first_cancel = Arc::new(tokio::sync::Notify::new());
        let release_cancel = Arc::new(tokio::sync::Notify::new());
        let chat_count = chats.clone();
        let cancel_count = cancels.clone();
        let observed_cancel = first_cancel.clone();
        let cancel_release = release_cancel.clone();
        let app = axum::Router::new()
            .route("/agents/edge", post(|| async { axum::Json(serde_json::json!({"ok": true})) }))
            .route("/sessions", post(|| async { axum::Json(serde_json::json!({"session_id": "sigint-session"})) }))
            .route("/skills", get(|| async { axum::Json(serde_json::json!({"skills": [], "total": 0, "limit": 100, "next_cursor": null})) }))
            .route("/models", get(|| async { pipe_model_catalog() }))
            .route("/chat/stream", post(move || {
                chat_count.fetch_add(1, Ordering::SeqCst);
                async move {
                    let stream = futures_util::stream::once(async {
                        Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"type\":\"session_info\",\"session_id\":\"sigint-session\",\"run_id\":\"sigint-run\"}\n\ndata: {\"type\":\"text_delta\",\"content\":\"observed partial\"}\n\ndata: {\"type\":\"usage\",\"input_tokens\":2,\"output_tokens\":1}\n\ndata: {\"type\":\"warning\",\"message\":\"force-writer-enospc\"}\n\ndata: {\"type\":\"session_info\",\"session_id\":\"changed-session\",\"run_id\":\"sigint-run\"}\n\n"))
                    }).chain(futures_util::stream::pending());
                    Response::builder()
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER, astra_server_types::AGENT_INTERACTION_API_MAJOR)
                        .body(Body::from_stream(stream)).unwrap()
                }
            }))
            .route("/chat/runs/{run_id}", delete(move |Path(run): Path<String>| {
                let cancels = cancel_count.clone();
                let started = observed_cancel.clone();
                let release = cancel_release.clone();
                async move {
                    assert_eq!(run, "sigint-run", "cleanup stays on the captured exact run");
                    if cancels.fetch_add(1, Ordering::SeqCst) == 0 {
                        started.notify_one();
                        release.notified().await;
                    }
                    axum::Json(serde_json::json!({"execution_settled": true}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let home = tempfile::tempdir().unwrap();
        let events = home.path().join("events.jsonl");
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"));
        command
            .args([
                "--api-url",
                &base,
                "--profile",
                "synthetic-double-failure",
                "--model",
                "mock-model",
                "--no-instructions",
                "chat",
                "--json",
                "--no-resume",
                "--permission-mode",
                "bypass",
                "--stream-events",
            ])
            .arg(&events)
            .args(["-m", "synthetic double failure"])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("XDG_CACHE_HOME", home.path().join(".cache"))
            .env("XDG_DATA_HOME", home.path().join(".local/share"))
            .env("ASTRA_ACCESS_TOKEN", "synthetic-offline-signal-test")
            .env("ASTRA_API_URL", &base)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if inject_enospc {
            let mut preload = fault.as_os_str().to_os_string();
            if let Some(inherited) = std::env::var_os("LD_PRELOAD") {
                preload.push(":");
                preload.push(inherited);
            }
            command.env("LD_PRELOAD", preload);
        }
        let child = command.spawn().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), first_cancel.notified())
            .await
            .expect("identity failure must reach exact-run cleanup before SIGINT");
        let pid = nix::unistd::Pid::from_raw(child.id().unwrap() as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        release_cancel.notify_one();
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output())
                .await
                .expect("bounded double-failure exit")
                .unwrap();
        server.abort();
        assert_eq!(output.status.signal(), None);
        assert_eq!(
            output.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("terminal failure JSON");
        let diagnostic = format!("inject_enospc={inject_enospc}; terminal={json}");
        assert_eq!(json["final_state"], "failed", "{diagnostic}");
        assert_eq!(json["server_terminal_unverified"], true, "{diagnostic}");
        assert_eq!(json["success"], false, "{diagnostic}");
        assert_eq!(json["run_id"], "sigint-run", "{diagnostic}");
        assert_eq!(json["text"], "observed partial", "{diagnostic}");
        let error = json["error"].as_str().unwrap();
        assert!(
            error.contains("server session identity changed within one request"),
            "{diagnostic}"
        );
        assert_eq!(
            error.contains("stream-event file write failed"),
            inject_enospc,
            "{diagnostic}"
        );
        if inject_enospc {
            assert!(
                error.contains(&std::io::Error::from_raw_os_error(libc::ENOSPC).to_string()),
                "{diagnostic}"
            );
        }
        assert_eq!(
            chats.load(Ordering::SeqCst),
            1,
            "never replay the submitted turn"
        );
        assert_eq!(
            cancels.load(Ordering::SeqCst),
            2,
            "facade and interrupt settle the same exact owner"
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_color_chat_preserves_plain_progress_and_answer() {
    use axum::{
        Router,
        routing::{get, post},
    };
    use nix::pty::{Winsize, openpty};
    use serde_json::json;
    use std::io::Read;
    use std::process::Stdio;

    let app = Router::new()
        .route("/agents/edge", post(|| async { axum::Json(json!({"ok": true})) }))
        .route("/sessions", post(|| async { axum::Json(json!({"id": "color-session"})) }))
        .route("/skills", get(|| async { axum::Json(json!({"skills": [], "total": 0, "limit": 100, "next_cursor": null})) }))
        .route("/models", get(|| async { pipe_model_catalog() }))
        .route("/chat/stream", post(|| async {
            let receipt = astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt::new(
                "color-run", 1, 1, 1, 0,
                astra_turn_core::tool_ledger_receipt::ToolLedgerResultClassCounts { succeeded: 1, ..Default::default() },
                1, astra_turn_core::tool_ledger_receipt::EMPTY_TOOL_LEDGER_ROOT, true,
            );
            let events = [
                json!({"type": "session_info", "session_id": "color-session", "run_id": "color-run"}),
                json!({"type": "tool_call_start", "call_id": "color-tool", "tool": "web_fetch", "arguments": {"url": "https://example.invalid/color-probe"}, "transport": "server_local", "executor": {"kind": "server_local"}}),
                json!({"type": "tool_call_end", "call_id": "color-tool", "tool": "web_fetch", "status": "completed", "output": "# Offline fixture\n\ncolor-probe", "duration_ms": 125, "transport": "server_local", "executor": {"kind": "server_local"}}),
                json!({"type": "text_delta", "content": "offline color answer"}),
                json!({"type": "text_done", "full_text": "offline color answer"}),
                json!({"type": "usage", "input_tokens": 1, "output_tokens": 3}),
                json!({"type": "run_finished", "run_id": "color-run", "status": "completed", "owner_generation": 1}),
                json!({"type": "turn_complete", "has_tool_calls": true, "continuation_owner": "server", "tool_calls_count": 1, "observation_tool_calls_count": 1, "tools_used": ["web_fetch"], "llm_rounds": 1, "tool_ledger_receipt": receipt}),
            ];
            let body = events.into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>() + "data: [DONE]\n\n";
            ([("content-type", "text/event-stream"), (astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER, astra_server_types::AGENT_INTERACTION_API_MAJOR)], body)
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let sgr = regex::Regex::new("\x1b\\[[0-9;:]*m").unwrap();
    for (case, no_color, flag) in [
        ("colored", None, false),
        ("flag", None, true),
        ("set", Some("1"), false),
        ("empty", Some(""), false),
    ] {
        let home = tempfile::tempdir().unwrap();
        let pty = openpty(
            Some(&Winsize {
                ws_row: 24,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra"));
        command.args([
            "--api-url",
            &base_url,
            "--profile",
            "pipe-e2e",
            "--model",
            "mock-model",
            "--no-instructions",
        ]);
        command.arg("chat");
        if flag {
            command.arg("--no-color");
        }
        command
            .args([
                "--no-resume",
                "--permission-mode",
                "bypass",
                "-m",
                "exercise offline color rendering",
            ])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("ASTRA_LOCAL_STATE_ROOT", home.path().join(".astra"))
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("XDG_CACHE_HOME", home.path().join(".cache"))
            .env("XDG_DATA_HOME", home.path().join(".local/share"))
            .env("ASTRA_ACCESS_TOKEN", "pipe-e2e-token")
            .env("ASTRA_API_URL", &base_url)
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::from(pty.slave))
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        if let Some(value) = no_color {
            command.env("NO_COLOR", value);
        }
        // stderr is a real terminal, so the control cannot pass merely because
        // command_router automatically disables styles for redirected stderr.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let stderr = tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::from(pty.master);
            let mut bytes = Vec::new();
            let result = file.read_to_end(&mut bytes);
            if let Err(error) = result {
                assert_eq!(error.raw_os_error(), Some(libc::EIO));
            }
            String::from_utf8(bytes).unwrap()
        });
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            command.spawn().unwrap().wait_with_output(),
        )
        .await
        .unwrap()
        .unwrap();
        drop(command);
        let stderr = tokio::time::timeout(std::time::Duration::from_secs(5), stderr)
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: status={} stdout={:?} stderr={stderr:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.contains("color-probe"),
            "{case}: tool progress missing: {stderr}"
        );
        assert_eq!(
            sgr.is_match(&stderr),
            case == "colored",
            "{case}: {stderr:?}"
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            stdout.trim(),
            "offline color answer",
            "{case}: stdout contract changed"
        );
    }
    server.abort();
}
