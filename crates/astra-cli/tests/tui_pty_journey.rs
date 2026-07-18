#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};

const CPR_REQUEST: &[u8] = b"\x1b[6n";
const CPR_RESPONSE: &[u8] = b"\x1b[1;1R";
const DA1_REQUEST: &[u8] = b"\x1b[c";
const DA1_RESPONSE_WITHOUT_SIXEL: &[u8] = b"\x1b[?1;2c";
// Full-workspace nextest runs contend for CPU and linker I/O even though each
// PTY and mock server is isolated. Keep UI transitions bounded, but do not use
// a sub-suite timing assumption as the product contract.
const UI_TRANSITION_TIMEOUT: Duration = Duration::from_secs(10);

/// A PTY journey owns a controlling terminal and flips the child into raw
/// mode. Keep those process-level terminal journeys serial even though their
/// homes and mock servers are isolated; parallel unit tests remain unaffected.
fn pty_journey_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// A real pseudoterminal around the shipped `astra` binary.
///
/// The harness answers the two terminal capability queries used before the
/// async input reader starts. Everything after that is driven as bytes through
/// the same TTY boundary a user has; no product-only test command or alternate
/// event loop is involved.
struct PtyAstra {
    child: Child,
    writer: File,
    output_rx: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
    output: Vec<u8>,
    screen: vt100::Parser,
    cpr_replies: usize,
    da1_replies: usize,
}

impl PtyAstra {
    fn spawn(home: &std::path::Path, api_url: &str) -> Self {
        let size = Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&size), None).expect("open pseudoterminal");
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        let stdin = slave.try_clone().expect("clone PTY slave for stdin");
        let stdout = slave.try_clone().expect("clone PTY slave for stdout");

        let mut child = Command::new(env!("CARGO_BIN_EXE_astra"));
        child
            .args([
                "--api-url",
                api_url,
                "--profile",
                "pty-journey",
                "--model",
                "mock-model",
                "--bare",
                "--no-instructions",
                "interactive",
            ])
            .current_dir(home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            // This is the documented gateway hand-off contract. It bypasses
            // interactive login validation but keeps normal request auth and
            // the full chat turn path intact.
            .env("ASTRA_ACCESS_TOKEN", "pty-journey-token")
            .env("ASTRA_API_URL", api_url)
            .env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .env_remove("ZELLIJ_SESSION_NAME")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(slave));
        // Connecting stdio to a PTY is not sufficient: crossterm reads from
        // the process controlling terminal. Start a fresh session and attach
        // fd 0 (already redirected to the slave) before exec.
        unsafe {
            child.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = child.spawn().expect("spawn Astra in PTY");

        let writer = master.try_clone().expect("clone PTY master for input");
        let (output_tx, output_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut reader = master;
            let mut chunk = [0_u8; 8 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) if output_tx.send(chunk[..read].to_vec()).is_err() => break,
                    Ok(_) => {}
                }
            }
        });

        Self {
            child,
            writer,
            output_rx,
            reader: Some(reader),
            output: Vec::new(),
            screen: vt100::Parser::new(size.ws_row, size.ws_col, 0),
            cpr_replies: 0,
            da1_replies: 0,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write PTY input");
        self.writer.flush().expect("flush PTY input");
    }

    fn signal(&self, signal: nix::sys::signal::Signal) {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.child.id() as i32), signal)
            .expect("signal Astra PTY child");
    }

    fn wait_for(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self.current_screen().contains(needle) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll Astra child") {
                panic!(
                    "Astra exited before rendering {needle:?} ({status})\n{}",
                    self.screen_diagnostic()
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for {needle:?}\n{}",
                self.screen_diagnostic()
            );
            self.receive(remaining.min(Duration::from_millis(100)));
        }
    }

    fn receive(&mut self, timeout: Duration) {
        match self.output_rx.recv_timeout(timeout) {
            Ok(chunk) => {
                self.screen.process(&chunk);
                self.output.extend_from_slice(&chunk);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // PTY EOF/EIO can race process reaping by a few milliseconds.
                // Wait briefly so failures report the real exit status and
                // terminal tail instead of a misleading "still running".
                let deadline = Instant::now() + Duration::from_millis(500);
                loop {
                    if self.child.try_wait().expect("poll Astra child").is_some() {
                        return;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "PTY output closed while Astra was still running\n{}",
                        self.output_tail()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        self.answer_terminal_queries();
    }

    fn answer_terminal_queries(&mut self) {
        let cpr_requests = count_bytes(&self.output, CPR_REQUEST);
        while self.cpr_replies < cpr_requests {
            self.write(CPR_RESPONSE);
            self.cpr_replies += 1;
        }
        let da1_requests = count_bytes(&self.output, DA1_REQUEST);
        while self.da1_replies < da1_requests {
            self.write(DA1_RESPONSE_WITHOUT_SIXEL);
            self.da1_replies += 1;
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll Astra child") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "Astra did not exit\n{}",
                self.output_tail()
            );
            self.receive(Duration::from_millis(50));
        }
    }

    fn output_tail(&self) -> String {
        let text = String::from_utf8_lossy(&self.output).replace('\x1b', "<ESC>");
        text.chars()
            .rev()
            .take(6_000)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    }

    fn current_screen(&self) -> String {
        self.screen.screen().contents()
    }

    fn screen_diagnostic(&self) -> String {
        format!(
            "current screen:\n{}\n\nraw PTY tail:\n{}",
            self.current_screen(),
            self.output_tail()
        )
    }
}

impl Drop for PtyAstra {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn seed_trusted_workspace(home: &std::path::Path) {
    let workspace = home
        .canonicalize()
        .expect("canonical temporary workspace")
        .to_string_lossy()
        .into_owned();
    let astra_home = home.join(".astra");
    std::fs::create_dir_all(&astra_home).expect("create isolated Astra home");
    let ledger = serde_json::json!({
        "version": 1,
        "workspaces": {
            workspace: {
                "trust": "trusted",
                "trusted_at": "2026-07-13T00:00:00Z"
            }
        }
    });
    std::fs::write(
        astra_home.join("trusted_workspaces.json"),
        serde_json::to_vec_pretty(&ledger).expect("serialize workspace trust ledger"),
    )
    .expect("write workspace trust ledger");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sighup_while_idle_converges_through_tui_shutdown() {
    let _journey = pty_journey_lock().lock().await;
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), "http://127.0.0.1:9");

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.signal(nix::sys::signal::Signal::SIGHUP);

    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(
        status.success(),
        "idle SIGHUP must request graceful TUI convergence, got {status}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sighup_during_an_active_turn_converges_through_tui_shutdown() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::Slow,
    )
    .await
    .expect("start scripted slow LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"keep_this_turn_active_until_shutdown\r");
    astra.wait_for("Sending", UI_TRANSITION_TIMEOUT);

    astra.signal(nix::sys::signal::Signal::SIGHUP);
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(
        status.success(),
        "SIGHUP must request graceful TUI convergence, got {status}"
    );
}

fn is_agent_journey_child_request(request: &serde_json::Value) -> bool {
    request
        .get("agent_type")
        .and_then(serde_json::Value::as_str)
        == Some("general-purpose")
        && request
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .and_then(|messages| {
                messages.iter().rev().find(|message| {
                    message.get("role").and_then(serde_json::Value::as_str) == Some("user")
                })
            })
            .and_then(|message| message.get("content"))
            .and_then(serde_json::Value::as_str)
            == Some(astra_cli::cli::mock_llm::AGENT_JOURNEY_CHILD_TASK)
}

fn summarize_mock_request(request: &serde_json::Value) -> String {
    let edge_tool_names = request
        .get("edge_tools")
        .and_then(serde_json::Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.pointer("/function/name")
                        .and_then(serde_json::Value::as_str)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let tool_result_names = request
        .get("tool_results")
        .and_then(serde_json::Value::as_array)
        .map(|results| {
            results
                .iter()
                .map(|result| {
                    result
                        .get("name")
                        .or_else(|| result.get("tool"))
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| {
                            result
                                .get("tool_call_id")
                                .and_then(serde_json::Value::as_str)
                        })
                        .unwrap_or("<unknown>")
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let assistant_tool_calls = request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter(|message| {
                    message.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                })
                .flat_map(|message| {
                    message
                        .get("tool_calls")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .filter_map(|call| {
                    call.pointer("/function/name")
                        .and_then(serde_json::Value::as_str)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    format!(
        "agent_id={:?} agent_type={:?} edge_tools={edge_tool_names:?} assistant_tool_calls={assistant_tool_calls:?} tool_results={tool_result_names:?}",
        request.get("agent_id").and_then(serde_json::Value::as_str),
        request
            .get("agent_type")
            .and_then(serde_json::Value::as_str),
    )
}

async fn wait_for_agent_journey_child_request(
    mock: &astra_cli::cli::mock_llm::MockLlmServer,
    astra: &mut PtyAstra,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let requests = mock.received_requests();
        if requests.iter().any(is_agent_journey_child_request) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mock did not receive a canonical child request; observed {} requests; latest request: {}\nPTY tail:\n{}",
            requests.len(),
            requests
                .last()
                .map(summarize_mock_request)
                .unwrap_or_else(|| "<none>".to_string()),
            astra.output_tail(),
        );
        // Keep servicing terminal capability queries while the child is
        // starting. A real terminal answers these concurrently; a PTY test
        // that only polls HTTP would deadlock the product on its next CPR.
        astra.receive(Duration::from_millis(10));
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_o_round_trip_preserves_composer_draft_in_a_real_pty() {
    let _journey = pty_journey_lock().lock().await;
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), "http://127.0.0.1:9");

    astra.wait_for("Enter send", Duration::from_secs(15));

    // A single token stays contiguous in the terminal byte stream even when
    // ratatui positions separately styled words with cursor movement codes.
    let draft = "draft_survives_transcript_round_trip";
    astra.write(draft.as_bytes());
    astra.wait_for(draft, UI_TRANSITION_TIMEOUT);

    astra.write(&[0x0f]); // Ctrl+O
    astra.wait_for("Main conversation · Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("filter:", Duration::from_secs(2));

    astra.write(&[0x0f]); // Ctrl+O
    astra.wait_for(draft, UI_TRANSITION_TIMEOUT);

    astra.write(&[0x15]); // Ctrl+U clears the restored draft.
    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_o_opens_during_an_active_turn_and_receives_live_completion() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::Slow,
    )
    .await
    .expect("start scripted slow LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"complete_this_live_transcript_journey\r");
    astra.wait_for("Sending", UI_TRANSITION_TIMEOUT);

    astra.write(&[0x0f]); // Ctrl+O while the HTTP turn is still pending.
    astra.wait_for("Main conversation · Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("successfully.", Duration::from_secs(10));

    astra.write(&[0x0f]);
    astra.wait_for("Enter send", Duration::from_secs(10));
    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_o_replays_tool_history_after_a_real_tool_turn() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::ToolThenComplete,
    )
    .await
    .expect("start scripted tool LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"/allow prompt\r");
    astra.wait_for("Mode → Ask", UI_TRANSITION_TIMEOUT);
    astra.write(b"exercise_tool_history_in_transcript\r");
    // The second mock response is only reachable after the real host has
    // accepted and executed the first response's tool request.
    astra.wait_for("Approval · Write File", Duration::from_secs(10));
    astra.write(b"\r");
    astra.wait_for("wrote the requested file", Duration::from_secs(10));

    astra.write(&[0x0f]); // Ctrl+O after the compact view observed the tool.
    astra.wait_for("Main conversation · Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Ran Write file", UI_TRANSITION_TIMEOUT);

    astra.write(&[0x0f]);
    astra.wait_for("Enter send", UI_TRANSITION_TIMEOUT);
    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_o_round_trip_preserves_a_live_tool_approval() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::ToolThenComplete,
    )
    .await
    .expect("start scripted tool LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"/allow prompt\r");
    astra.wait_for("Mode → Ask", UI_TRANSITION_TIMEOUT);
    astra.write(b"request_a_write_and_wait_for_my_approval\r");
    astra.wait_for("Approval · Write File", Duration::from_secs(10));

    astra.write(&[0x0f]); // Ctrl+O while approval owns the bottom pane.
    astra.wait_for("Main conversation · Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("write_file", UI_TRANSITION_TIMEOUT);

    astra.write(&[0x0f]);
    astra.wait_for("Approval · Write File", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r"); // The focused Yes action approves exactly this request.
    astra.wait_for("wrote the requested file", Duration::from_secs(10));

    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_g_reopens_a_child_transcript_after_completion() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
    )
    .await
    .expect("start scripted parent and child LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"delegate_one_child_and_keep_it_observable\r");
    wait_for_agent_journey_child_request(&mock, &mut astra, UI_TRANSITION_TIMEOUT).await;

    astra.write(&[0x07]); // Ctrl+G while the child response is still pending.
    astra.wait_for("Conversations", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Mock child review", UI_TRANSITION_TIMEOUT);

    astra.write(b"1\r"); // Numeric selection addresses the first child, not the root tab.
    astra.wait_for("Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("child_evidence_visible", Duration::from_secs(10));

    // Ctrl+O always focuses the retained root conversation without destroying
    // the child workspace.
    astra.write(&[0x0f]);
    astra.wait_for(
        "Parent synthesized the child evidence",
        Duration::from_secs(10),
    );

    // Completed children remain addressable from the same conversation
    // navigator. Reopening must hydrate the stored transcript prefix rather
    // than showing only events emitted after the view was opened.
    astra.write(&[0x07]);
    astra.wait_for("Conversations", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Mock child review", UI_TRANSITION_TIMEOUT);
    astra.write(b"1\r");
    astra.wait_for("Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("child_evidence_visible", UI_TRANSITION_TIMEOUT);
    assert_eq!(
        mock.received_requests()
            .iter()
            .filter(|request| is_agent_journey_child_request(request))
            .count(),
        1,
        "one canonical spawn result must advance the parent instead of repeating delegation"
    );
}
