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
            "--bare",
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
        .expect("read Astra stdout")
        .unwrap_or_else(|| {
            panic!("Astra exited before binding the streamed run; stdout: {stdout}")
        });
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
