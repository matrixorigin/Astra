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

    fn wait_for_absent(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.current_screen().contains(needle) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll Astra child") {
                panic!(
                    "Astra exited before clearing {needle:?} ({status})\n{}",
                    self.screen_diagnostic()
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for {needle:?} to clear\n{}",
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

fn selected_task_slot(screen: &str) -> Option<String> {
    screen.lines().find_map(|line| {
        let numbered = line.trim_start().strip_prefix('›')?.trim_start();
        let (ordinal, _) = numbered.split_once('.')?;
        ordinal.parse::<usize>().ok()?;
        numbered
            .split_once("slot ")
            .map(|(_, slot)| slot.trim().to_string())
    })
}

fn select_task_slot(astra: &mut PtyAstra, target: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let before = selected_task_slot(&astra.current_screen())
            .expect("task panel has one selected stable row");
        if before == target {
            return;
        }
        astra.write(b"\x1b[B");
        loop {
            astra.receive(Duration::from_millis(50));
            if selected_task_slot(&astra.current_screen()).as_deref() != Some(before.as_str()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "task selection did not move toward {target}\n{}",
                astra.current_screen()
            );
        }
        assert!(
            Instant::now() < deadline,
            "could not select task slot {target}\n{}",
            astra.current_screen()
        );
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_projects_stopping_until_a_slow_turn_settles() {
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
    astra.write(b"hold this turn open\r");
    astra.wait_for("Working", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x03]); // Ctrl+C through the real raw-mode input boundary.

    astra.wait_for("Stopping", UI_TRANSITION_TIMEOUT);
    assert!(
        !astra.current_screen().contains("Working"),
        "the accepted stop intent must replace the prior activity projection\n{}",
        astra.screen_diagnostic()
    );
    astra.wait_for("Enter send", Duration::from_secs(15));
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

fn is_fanout_journey_child_request(request: &serde_json::Value) -> bool {
    request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|content| {
            astra_cli::cli::mock_llm::FANOUT_JOURNEY_CHILD_TASKS.contains(&content)
        })
}

fn is_fanout_reconciliation_request(request: &serde_json::Value) -> bool {
    request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        == Some(astra_turn_core::chat_turn_edge_profile::RUNTIME_RECONCILIATION_USER_ENVELOPE)
}

fn is_fanout_status_question(request: &serde_json::Value) -> bool {
    request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        == Some(astra_cli::cli::mock_llm::FANOUT_JOURNEY_STATUS_QUESTION)
}

fn is_fanout_root_request(request: &serde_json::Value) -> bool {
    !is_fanout_journey_child_request(request)
}

async fn wait_for_three_fanout_children(
    mock: &astra_cli::cli::mock_llm::MockLlmServer,
    astra: &mut PtyAstra,
) {
    let deadline = tokio::time::Instant::now() + UI_TRANSITION_TIMEOUT;
    loop {
        let count = mock
            .received_requests()
            .iter()
            .filter(|request| is_fanout_journey_child_request(request))
            .count();
        if count == 3 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected three fanout child requests, got {count}\n{}",
            astra.screen_diagnostic()
        );
        astra.receive(Duration::from_millis(25));
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

    // Match the established Claude Code task-management mental model: while
    // detached work is live, the footer advertises the Shift+Down route and
    // that exact key opens the background-task manager.
    astra.wait_for("Shift+↓ manage", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b[1;2B"); // xterm Shift+Down
    astra.wait_for("  Tasks", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Mock child review", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b"); // close the manager before opening Conversations
    astra.wait_for_absent("  Tasks", UI_TRANSITION_TIMEOUT);

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_fanout_stays_observable_and_synthesizes_once_after_full_settlement() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::FanoutThenComplete,
    )
    .await
    .expect("start scripted fanout LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"launch_three_reviews_as_one_group\r");
    wait_for_three_fanout_children(&mock, &mut astra).await;
    astra.wait_for("↳ Work · Three mock reviews", UI_TRANSITION_TIMEOUT);
    astra.wait_for("parent waits for the complete group", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Shift+↓ manage", UI_TRANSITION_TIMEOUT);

    // The first two children settle while the third remains deliberately
    // blocked. Neither completion may advance the parent model; the runtime
    // projection, not another analysis turn, keeps the user informed.
    assert_eq!(
        mock.received_requests()
            .iter()
            .filter(|request| is_fanout_root_request(request))
            .count(),
        2,
        "tool discovery and fanout launch are the only parent requests before full settlement"
    );

    astra.write(b"\x1b[1;2B");
    astra.wait_for("Tasks", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Three mock reviews", UI_TRANSITION_TIMEOUT);
    astra.wait_for("slot 1: Mock review 1", UI_TRANSITION_TIMEOUT);
    astra.wait_for("slot 2: Mock review 2", UI_TRANSITION_TIMEOUT);
    astra.wait_for("slot 3: Mock review 3", UI_TRANSITION_TIMEOUT);
    let selected_before_move = selected_task_slot(&astra.current_screen())
        .expect("task panel has one selected stable row");
    astra.write(b"\x1b[B");
    let selection_deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    let selected_after_move = loop {
        astra.receive(Duration::from_millis(50));
        if let Some(selected) = selected_task_slot(&astra.current_screen())
            && selected != selected_before_move
        {
            break selected;
        }
        assert!(
            Instant::now() < selection_deadline,
            "Down did not move the stable task selection\n{}",
            astra.current_screen()
        );
    };

    let refresh_deadline = Instant::now() + Duration::from_millis(1_700);
    while Instant::now() < refresh_deadline {
        astra.receive(Duration::from_millis(50));
    }
    let refreshed = astra.current_screen();
    let first = refreshed
        .find("slot 1: Mock review 1")
        .expect("first fanout row");
    let second = refreshed
        .find("slot 2: Mock review 2")
        .expect("second fanout row");
    let third = refreshed
        .find("slot 3: Mock review 3")
        .expect("third fanout row");
    assert!(
        first < second && second < third,
        "rows jumped after refresh:\n{refreshed}"
    );
    assert_eq!(
        selected_task_slot(&refreshed).as_deref(),
        Some(selected_after_move.as_str()),
        "selected stable task identity changed during refresh:\n{refreshed}"
    );
    assert_eq!(
        mock.received_requests()
            .iter()
            .filter(|request| is_fanout_root_request(request))
            .count(),
        2,
        "partial child settlement must not trigger parent analysis"
    );
    assert_eq!(
        mock.received_requests()
            .iter()
            .filter(|request| is_fanout_reconciliation_request(request))
            .count(),
        0,
        "foreground fan-in must not create a detached reconciliation turn"
    );
    astra.write(b"\x1b");

    astra.wait_for(
        "Parent synthesized one terminal fanout group exactly once.",
        Duration::from_secs(10),
    );
    let received = mock.received_requests();
    let root_requests = received
        .iter()
        .filter(|request| is_fanout_root_request(request))
        .collect::<Vec<_>>();
    assert_eq!(
        root_requests.len(),
        3,
        "the full fanout result must produce one and only one parent synthesis; root requests: {:#?}",
        root_requests
            .iter()
            .map(|request| summarize_mock_request(request))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        received
            .iter()
            .filter(|request| is_fanout_reconciliation_request(request))
            .count(),
        0,
        "structured foreground completion must stay on the original parent turn"
    );
    assert!(
        !String::from_utf8_lossy(&astra.output)
            .contains("Fanout did not return a usable launch receipt"),
        "foreground fan-in must not paint a transient transport failure while the runtime owns live agents\n{}",
        astra.current_screen()
    );

    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_status_guidance_uses_canonical_group_truth_and_never_claims_settlement() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::FanoutThenComplete,
    )
    .await
    .expect("start scripted fanout LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"launch_then_ask_foreground_status\r");
    wait_for_three_fanout_children(&mock, &mut astra).await;
    astra.wait_for("parent waits for the complete group", UI_TRANSITION_TIMEOUT);

    // Let the first two deterministic children settle while the third remains
    // blocked for six seconds, then ask through the still-active composer.
    let partial_deadline = Instant::now() + Duration::from_millis(1_100);
    while Instant::now() < partial_deadline {
        astra.receive(Duration::from_millis(25));
    }
    let guidance_submitted_at = Instant::now();
    astra.write(
        format!(
            "{}\r",
            astra_cli::cli::mock_llm::FANOUT_JOURNEY_STATUS_QUESTION
        )
        .as_bytes(),
    );
    astra.wait_for(
        "Guidance queued · Three mock reviews: 2/3 settled, 1 running",
        UI_TRANSITION_TIMEOUT,
    );
    assert!(
        guidance_submitted_at.elapsed() < Duration::from_secs(2),
        "active guidance needs an immediate runtime receipt instead of waiting for foreground fan-in"
    );
    astra.wait_for(
        "Astra knows Three mock reviews completed as one foreground work group.",
        Duration::from_secs(10),
    );

    let status_requests = mock
        .received_requests()
        .into_iter()
        .filter(is_fanout_status_question)
        .collect::<Vec<_>>();
    assert_eq!(
        status_requests.len(),
        1,
        "one status question gets one analysis"
    );
    let status_request = &status_requests[0];
    let active_work = status_request
        .pointer("/edge_profile/runtime_volatile_injections")
        .and_then(serde_json::Value::as_array)
        .and_then(|injections| {
            injections
                .iter()
                .find(|injection| injection["kind"] == "active_work_snapshot")
        })
        .expect("active guidance must carry canonical typed group truth");
    assert_eq!(active_work["delivery_class"], "required_context");
    assert_eq!(
        active_work["payload"]["authority"],
        "runtime_required_context"
    );
    assert_eq!(
        active_work["payload"]["schema"],
        "active_work_guidance_context.v1"
    );
    let snapshot = &active_work["payload"]["snapshots"][0];
    assert_eq!(snapshot["schema"], "active_work_snapshot.v1");
    assert_eq!(snapshot["authority"], "run_control_provider");
    assert_eq!(
        snapshot["projection_state"],
        "superseded_by_newer_producer_observation"
    );
    let observation = &snapshot["work_unit_observations"][0];
    assert_eq!(observation["id"], "mock-review-group");
    assert_eq!(observation["kind"], "agent_fanout");
    assert_eq!(observation["status"], "completed");
    assert!(
        !String::from_utf8_lossy(&astra.output).contains("All three reviewers completed"),
        "a non-terminal group must never be presented as settled"
    );
    assert!(
        !String::from_utf8_lossy(&astra.output)
            .contains("are running as one background work group"),
        "the delayed model boundary must not repeat the stale submission-time status"
    );

    astra.wait_for(
        "terminal fanout group exactly once.",
        Duration::from_secs(12),
    );
    let received = mock.received_requests();
    assert_eq!(
        received
            .iter()
            .filter(|request| is_fanout_status_question(request))
            .count(),
        1,
        "terminal settlement must not replay the status question"
    );
    assert_eq!(
        received
            .iter()
            .filter(|request| is_fanout_root_request(request))
            .count(),
        3,
        "tool discovery, fanout launch, and one combined terminal status/synthesis are the only parent boundaries"
    );
    assert_eq!(
        received
            .iter()
            .filter(|request| is_fanout_reconciliation_request(request))
            .count(),
        0,
        "foreground fan-in must remain on its original parent after a status guidance boundary"
    );

    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_fanout_slot_preserves_its_cause_and_still_synthesizes_once() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::FanoutPartialThenComplete,
    )
    .await
    .expect("start partial fanout LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"launch_three_reviews_with_one_unhappy_child\r");
    wait_for_three_fanout_children(&mock, &mut astra).await;
    astra.wait_for("↳ Work · Three mock reviews", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b[1;2B");
    astra.wait_for("slot 2: Mock review 2", UI_TRANSITION_TIMEOUT);
    astra.wait_for("failed", UI_TRANSITION_TIMEOUT);

    // Select the failed slot by stable identity and verify that its distinct
    // cause is inspectable while the final slow slot is still running.
    select_task_slot(&mut astra, "2: Mock review 2", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r");
    astra.wait_for(
        "fanout_child_2_failed_with_distinct_cause",
        UI_TRANSITION_TIMEOUT,
    );
    assert_eq!(
        mock.received_requests()
            .iter()
            .filter(|request| is_fanout_root_request(request))
            .count(),
        2,
        "one child failure must not trigger partial parent analysis"
    );
    astra.write(b"\x1b");
    astra.wait_for("  Tasks", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b");
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);

    astra.wait_for(
        "Parent synthesized the available 2/3 fanout evidence exactly once.",
        Duration::from_secs(10),
    );
    let received = mock.received_requests();
    let root_requests = received
        .iter()
        .filter(|request| is_fanout_root_request(request))
        .collect::<Vec<_>>();
    assert_eq!(root_requests.len(), 3, "partial fan-in gets one synthesis");
    let final_request = root_requests
        .last()
        .expect("final parent request")
        .to_string();
    assert!(
        final_request.contains("\\\"completed\\\":2") || final_request.contains("\"completed\":2"),
        "canonical partial aggregate must disclose 2 completed slots: {final_request}"
    );
    assert!(
        final_request.contains("\\\"failed\\\":1") || final_request.contains("\"failed\":1"),
        "canonical partial aggregate must disclose one failed slot: {final_request}"
    );
    assert_eq!(
        received
            .iter()
            .filter(|request| is_fanout_reconciliation_request(request))
            .count(),
        0,
        "partial foreground result remains on the original parent"
    );
    assert!(
        !String::from_utf8_lossy(&astra.output)
            .contains("Fanout did not return a usable launch receipt"),
        "an unhappy child must not fabricate a launch-transport failure"
    );

    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_b_promotes_the_whole_fanout_and_wakes_once_after_settlement() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::FanoutThenComplete,
    )
    .await
    .expect("start scripted fanout LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"launch_then_explicitly_background_the_group\r");
    wait_for_three_fanout_children(&mock, &mut astra).await;
    astra.wait_for("↳ Work · Three mock reviews", UI_TRANSITION_TIMEOUT);

    astra.write(&[0x02]); // Ctrl+B is the only lifecycle handoff.
    astra.wait_for(
        "Backgrounded mock-review-group (3 agents)",
        UI_TRANSITION_TIMEOUT,
    );
    astra.wait_for("one update after the group settles", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Shift+↓ inspect", UI_TRANSITION_TIMEOUT);
    astra.wait_for(
        "Three mock reviews finished · 3/3 completed",
        UI_TRANSITION_TIMEOUT,
    );

    // Backgrounding does not replace the conversation with a panel. The same
    // advertised Shift+Down route opens the now-detached group on demand.
    astra.write(b"\x1b[1;2B");
    astra.wait_for("  Tasks", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Three mock reviews", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b");

    astra.wait_for(
        "Parent reconciled one terminal fanout group exactly once.",
        Duration::from_secs(12),
    );
    let received = mock.received_requests();
    let reconciliation_requests = received
        .iter()
        .filter(|request| is_fanout_reconciliation_request(request))
        .collect::<Vec<_>>();
    assert_eq!(
        reconciliation_requests.len(),
        1,
        "an explicitly backgrounded work group must wake exactly once"
    );
    assert_eq!(
        reconciliation_requests[0]
            .pointer("/edge_profile/runtime_reconciliation_turn")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "background wake must use the typed runtime reconciliation lane"
    );
    assert_eq!(
        received
            .iter()
            .filter(|request| is_fanout_root_request(request))
            .count(),
        3,
        "the cancelled foreground parent must not also synthesize the same group"
    );
    assert!(
        !String::from_utf8_lossy(&astra.output)
            .contains("Fanout did not return a usable launch receipt"),
        "Ctrl+B must not paint a transient false failure before the runtime-owned handoff"
    );

    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_group_is_queryable_before_its_single_terminal_wake() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::FanoutThenComplete,
    )
    .await
    .expect("start scripted fanout LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Enter send", Duration::from_secs(15));
    astra.write(b"launch_then_ask_about_background_state\r");
    wait_for_three_fanout_children(&mock, &mut astra).await;
    astra.wait_for("↳ Work · Three mock reviews", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x02]);
    astra.wait_for(
        "Backgrounded mock-review-group (3 agents)",
        UI_TRANSITION_TIMEOUT,
    );
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);

    astra.write(
        format!(
            "{}\r",
            astra_cli::cli::mock_llm::FANOUT_JOURNEY_STATUS_QUESTION
        )
        .as_bytes(),
    );
    astra.wait_for(
        "Astra knows Three mock reviews are running as one background work group.",
        UI_TRANSITION_TIMEOUT,
    );
    let status_requests = mock
        .received_requests()
        .into_iter()
        .filter(is_fanout_status_question)
        .collect::<Vec<_>>();
    assert_eq!(status_requests.len(), 1);
    let status_request = &status_requests[0];
    let active_work = status_request
        .pointer("/edge_profile/runtime_volatile_injections")
        .and_then(serde_json::Value::as_array)
        .and_then(|injections| {
            injections
                .iter()
                .find(|injection| injection["kind"] == "active_work_snapshot")
        })
        .expect("ordinary user questions receive typed runtime-owned work truth");
    assert_eq!(active_work["delivery_class"], "required_context");
    assert_eq!(active_work["payload"]["authority"], "runtime_producer");
    assert_eq!(active_work["payload"]["schema"], "active_work_snapshot.v1");
    let observation = &active_work["payload"]["work_unit_observations"][0];
    assert_eq!(observation["id"], "mock-review-group");
    assert_eq!(observation["kind"], "agent_fanout");
    assert_eq!(observation["status"], "running");
    assert_eq!(
        mock.received_requests()
            .iter()
            .filter(|request| is_fanout_reconciliation_request(request))
            .count(),
        0,
        "an active background group must not wake before its terminal boundary"
    );

    astra.wait_for(
        "Parent reconciled one terminal fanout group exactly once.",
        Duration::from_secs(12),
    );
    assert_eq!(
        mock.received_requests()
            .iter()
            .filter(|request| is_fanout_reconciliation_request(request))
            .count(),
        1,
        "the same group receives one terminal wake after becoming queryable"
    );

    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}
