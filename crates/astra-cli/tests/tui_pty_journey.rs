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
const LIVE_TEAM_API_URL_ENV: &str = "ASTRA_TUI_LIVE_API_URL";
const LIVE_TEAM_MODEL_ENV: &str = "ASTRA_TUI_LIVE_MODEL";
const LIVE_TEAM_ACCESS_TOKEN_ENV: &str = "ASTRA_TUI_LIVE_ACCESS_TOKEN";

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
        Self::spawn_with_config(home, api_url, "mock-model", "pty-journey-token", &[])
    }

    fn spawn_with_config(
        home: &std::path::Path,
        api_url: &str,
        model: &str,
        access_token: &str,
        launch_args: &[&str],
    ) -> Self {
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
            .args(launch_args)
            .args([
                "--api-url",
                api_url,
                "--profile",
                "pty-journey",
                "--model",
                model,
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
            .env("ASTRA_ACCESS_TOKEN", access_token)
            .env("ASTRA_API_URL", api_url)
            .env("TERM", "xterm-256color")
            .env_remove("ASTRA_CLI_CREDENTIALS_DIR")
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

    fn paste_and_submit(&mut self, text: &str, timeout: Duration) {
        // The journey is injecting a whole message, not simulating a human
        // typing one character at a time. Use the terminal's bracketed-paste
        // protocol so the application receives the same typed event as a
        // real terminal paste. Raw bulk bytes intentionally exercise the
        // fallback paste-burst detector, where Enter is briefly interpreted
        // as a pasted newline rather than a submit gesture.
        self.write(b"\x1b[200~");
        self.write(text.as_bytes());
        self.write(b"\x1b[201~");
        // Long pastes scroll the composer; its visible suffix confirms delivery.
        let suffix = text.chars().rev().take(120).collect::<Vec<_>>();
        let expected = suffix
            .into_iter()
            .rev()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let deadline = Instant::now() + timeout;
        loop {
            let visible = self
                .current_screen()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>();
            if visible.contains(&expected) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "composer did not accept {text:?}\n{}",
                self.screen_diagnostic()
            );
            self.receive(Duration::from_millis(25));
        }
        self.write(b"\r");
    }

    fn select_conversation(&mut self, name: &str) {
        self.write(&[0x07]);
        self.wait_for("Conversations", UI_TRANSITION_TIMEOUT);
        self.wait_for(name, UI_TRANSITION_TIMEOUT);
        let choice = self
            .current_screen()
            .lines()
            .find_map(|line| {
                let (prefix, _) = line.split_once(&format!(". {name}"))?;
                prefix.split_whitespace().last()?.parse::<usize>().ok()
            })
            .expect("conversation is selectable in the picker");
        self.write(format!("{choice}\r").as_bytes());
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
                self.drain_output_after_exit();
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

    fn drain_output_after_exit(&mut self) {
        // Once the child has exited, PTY EOF is authoritative. Join the sole
        // reader first, then consume every chunk it published; a wall-clock
        // grace period can lose the final post-terminal resume line on a busy
        // test host.
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .expect("join PTY output reader after child exit");
        }
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.screen.process(&chunk);
            self.output.extend_from_slice(&chunk);
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
            // Give the ordinary shutdown path a bounded opportunity to cancel
            // active execution before forcing cleanup of this owned process.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.child.id() as i32),
                nix::sys::signal::Signal::SIGHUP,
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.child.try_wait().ok().flatten().is_none() {
                if Instant::now() >= deadline {
                    let _ = self.child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
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

fn required_live_env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("ignored live PTY journey requires {name}"))
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

    astra.wait_for("Message Astra", Duration::from_secs(15));
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

    astra.wait_for("Message Astra", Duration::from_secs(15));
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
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_slow_response()
        .await
        .expect("start scripted slow LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.write(b"hold this turn open\r");

    // Synchronize on the request reaching the provider. A transient activity
    // label is presentation state, not proof that the turn is still live; on
    // a loaded workspace it can first be observed at the completion boundary.
    let request_deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().is_empty() {
        assert!(
            Instant::now() < request_deadline,
            "the slow turn never reached the provider\n{}",
            astra.screen_diagnostic()
        );
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    astra.write(&[0x03]); // Ctrl+C through the real raw-mode input boundary.

    astra.wait_for("Stopping", UI_TRANSITION_TIMEOUT);
    assert!(
        !astra.current_screen().contains("Working"),
        "the accepted stop intent must replace the prior activity projection\n{}",
        astra.screen_diagnostic()
    );
    mock.release_held_response();
    astra.wait_for("Message Astra", Duration::from_secs(15));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_o_round_trip_preserves_composer_draft_in_a_real_pty() {
    let _journey = pty_journey_lock().lock().await;
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), "http://127.0.0.1:9");

    astra.wait_for("Message Astra", Duration::from_secs(15));

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
async fn exit_after_a_completed_turn_prints_a_copyable_resume_command() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::Slow,
    )
    .await
    .expect("start scripted LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Message", Duration::from_secs(15));
    astra.write(b"create_a_resumable_session\r");
    astra.wait_for("successfully.", Duration::from_secs(10));
    // The completion text can be painted before the input surface has
    // returned to its idle state. Synchronize on the actual prompt before
    // injecting /exit so a loaded workspace cannot race the command parser.
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.write(b"/exit\r");
    astra.wait_for("Stopping", UI_TRANSITION_TIMEOUT);

    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
    let output = String::from_utf8_lossy(&astra.output);
    assert!(output.contains("Resume this session with:"), "{output}");
    assert!(output.contains("astra --resume mock-session"), "{output}");
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

    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.write(b"complete_this_live_transcript_journey\r");
    astra.wait_for("Sending", UI_TRANSITION_TIMEOUT);

    astra.write(&[0x0f]); // Ctrl+O while the HTTP turn is still pending.
    astra.wait_for("Main conversation · Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("successfully.", Duration::from_secs(10));

    astra.write(&[0x0f]);
    astra.wait_for("Message Astra", Duration::from_secs(10));
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

    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.write(b"/allow prompt\r");
    astra.wait_for("Mode → Ask", UI_TRANSITION_TIMEOUT);
    astra.write(b"exercise_tool_history_in_transcript\r");
    // The Server stream continues only after the real host has accepted and
    // executed its tool request and posted the exact callback.
    astra.wait_for("Approval · Write File", Duration::from_secs(10));
    astra.write(b"\r");
    astra.wait_for("wrote the requested file", Duration::from_secs(10));
    assert_committed_mock_write(&mock, home.path());

    astra.write(&[0x0f]); // Ctrl+O after the compact view observed the tool.
    astra.wait_for("Main conversation · Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Edited mock-output-astra-cli.txt", UI_TRANSITION_TIMEOUT);

    astra.write(&[0x0f]);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
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

    astra.wait_for("Message Astra", Duration::from_secs(15));
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
    assert_committed_mock_write(&mock, home.path());
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);

    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

fn assert_committed_mock_write(
    mock: &astra_cli::cli::mock_llm::MockLlmServer,
    workspace: &std::path::Path,
) {
    assert_eq!(
        std::fs::read_to_string(workspace.join("mock-output-astra-cli.txt")).unwrap(),
        "Output from astra-cli\n",
    );
    assert_eq!(mock.received_requests().len(), 1, "no client continuation");
    let callbacks = mock.tool_results();
    assert_eq!(callbacks.len(), 1, "one validated write callback");
    assert_eq!(callbacks[0]["status"], "completed");
}

async fn live_team_json(
    client: &reqwest::Client,
    api: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut request = client.request(method, format!("{api}{path}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    request
        .send()
        .await
        .expect("live API request")
        .error_for_status()
        .expect("live API accepted request")
        .json()
        .await
        .expect("live API JSON")
}

fn wait_for_live_session_id(astra: &mut PtyAstra, home: &std::path::Path) -> String {
    let store = astra_credentials::CredentialStore::with_path(home.join(".astra/credentials.json"));
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(credentials) = store.load()
            && let Some(id) = credentials
                .profiles
                .get("pty-journey")
                .and_then(|profile| profile.last_session_id.as_ref())
        {
            return id.clone();
        }
        assert!(
            Instant::now() < deadline,
            "CLI did not persist its admitted session identity"
        );
        astra.receive(Duration::from_millis(100));
    }
}

fn live_tool_json(value: &serde_json::Value) -> serde_json::Value {
    match value.as_str() {
        Some(text) => serde_json::from_str(text).expect("structured tool arguments"),
        None => value.clone(),
    }
}

async fn assert_live_team_round(
    astra: &mut PtyAstra,
    client: &reqwest::Client,
    api: &str,
    session_id: &str,
    team: &serde_json::Value,
    round: usize,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(180);
    let tree = loop {
        astra.receive(Duration::from_millis(25));
        let tree = live_team_json(
            client,
            api,
            reqwest::Method::GET,
            &format!("/sessions/{session_id}/runs"),
            None,
        )
        .await;
        let roots: Vec<_> = tree["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|run| run["depth"] == 0)
            .collect();
        if roots.len() == round && roots.iter().all(|run| run["status"] == "completed") {
            break tree;
        }
        assert!(
            roots.iter().all(|run| !matches!(
                run["status"].as_str(),
                Some("failed" | "cancelled" | "interrupted" | "paused")
            )),
            "live root did not deliver: {roots:?}"
        );
        assert!(
            Instant::now() < deadline,
            "live Team turn did not settle: {roots:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(tree["truncated"], false);
    let runs = tree["runs"].as_array().expect("durable run tree");
    let roots: Vec<_> = runs.iter().filter(|run| run["depth"] == 0).collect();
    assert_eq!(roots.len(), round, "each user turn owns one root");
    let root = roots
        .iter()
        .max_by_key(|run| run["created_at"].as_str().unwrap())
        .unwrap();
    assert_eq!(root["status"], "completed");
    let root_id = root["run_id"].as_str().unwrap();
    let children: Vec<_> = runs
        .iter()
        .filter(|run| run["parent_run_id"] == root_id)
        .collect();
    assert_eq!(children.len(), 2, "builder and reviewer each execute once");
    let root_projection = live_team_json(
        client,
        api,
        reqwest::Method::GET,
        &format!("/chat/runs/{root_id}/projection?recent_limit=500"),
        None,
    )
    .await;
    assert!(
        root_projection["run_event_high_watermark"]
            .as_i64()
            .unwrap()
            < 500,
        "bounded journey must retain its complete public event sequence"
    );
    let root_events = root_projection["recent_events"].as_array().unwrap();
    let mut member_ids = std::collections::BTreeMap::new();
    for spawn in root_events
        .iter()
        .filter(|event| event["type"] == "agent_spawned")
    {
        let profile = spawn["agent_type"].as_str().unwrap();
        let child_id = spawn["run_id"].as_str().unwrap();
        let child = children
            .iter()
            .find(|run| run["run_id"] == child_id)
            .unwrap();
        assert_eq!(child["agent_id"], spawn["agent_id"]);
        assert_eq!(spawn["parent_run_id"], root_id);
        assert_eq!(child["status"], "completed");
        assert_eq!(child["root_run_id"], root_id);
        assert!(
            member_ids
                .insert(profile.to_string(), child_id.to_string())
                .is_none()
        );
        let projection = live_team_json(
            client,
            api,
            reqwest::Method::GET,
            &format!("/chat/runs/{child_id}/projection?recent_limit=500"),
            None,
        )
        .await;
        assert!(projection["run_event_high_watermark"].as_i64().unwrap() < 500);
        let events = projection["recent_events"].as_array().unwrap();
        let facts = events
            .iter()
            .filter(|event| event["type"] == "explain_analyze")
            .map(|event| astra_turn_types::decode_explain_analyze_wire(event).unwrap())
            .collect::<Vec<_>>();
        assert!(facts.iter().all(|fact| fact.run_id == child_id));
        use astra_turn_types::{ExplainAnalyzeNodeKindV1, ExplainAnalyzeTransitionV1};
        for kind in [
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeNodeKindV1::ProviderAttempt,
            ExplainAnalyzeNodeKindV1::ToolCall,
        ] {
            assert!(
                facts.iter().any(|fact| fact.kind == kind
                    && fact.transition == ExplainAnalyzeTransitionV1::Finished),
                "{profile} must replay its own terminal {kind:?} facts"
            );
        }
        let expected: &[(&str, &str)] = if profile == "builder" {
            &[("read_file", "source.csv"), ("write_file", "report.json")]
        } else {
            &[
                ("read_file", "source.csv"),
                ("read_file", "report.json"),
                ("write_file", "review.json"),
            ]
        };
        for (tool, path) in expected {
            let (request, end) = events
                .iter()
                .filter(|event| {
                    event["type"] == "tool_request"
                        && event["tool"] == *tool
                        && live_tool_json(&event["args"])["path"]
                            .as_str()
                            .is_some_and(|value| std::path::Path::new(value).ends_with(path))
                })
                .find_map(|request| {
                    events
                        .iter()
                        .find(|event| {
                            event["type"] == "tool_call_end"
                                && event["call_id"] == request["request_id"]
                                && event["status"] == "completed"
                        })
                        .map(|end| (request, end))
                })
                .unwrap_or_else(|| panic!("{profile} must successfully {tool} {path}"));
            assert_eq!(request["run_id"], child_id);
            assert_eq!(end["success"], true);
            assert_eq!(end["transport"], "edge_ledger");
        }
    }
    assert_eq!(
        member_ids.keys().map(String::as_str).collect::<Vec<_>>(),
        ["builder", "reviewer"]
    );
    assert!(
        root_events
            .iter()
            .filter(|event| event["type"] == "tool_request" && event["run_id"] == root_id)
            .all(|event| !matches!(event["tool"].as_str(), Some("write_file" | "bash"))),
        "lead must delegate artifact production"
    );
    let builder_done = root_events
        .iter()
        .position(|event| {
            event["type"] == "agent_completed" && event["run_id"] == member_ids["builder"]
        })
        .expect("observed builder completion");
    let reviewer_started = root_events
        .iter()
        .position(|event| {
            event["type"] == "agent_spawned" && event["run_id"] == member_ids["reviewer"]
        })
        .unwrap();
    assert!(
        builder_done < reviewer_started,
        "reviewer consumes a settled builder result"
    );
    let resume = live_team_json(
        client,
        api,
        reqwest::Method::POST,
        &format!("/sessions/{session_id}/resume"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(
        resume["resume_bundle"]["projections"]["provider"]["payload"]["agent_profile_selection"]["team_id"],
        team["team_id"]
    );
    assert_eq!(
        resume["resume_bundle"]["projections"]["provider"]["payload"]["agent_profile_selection"]["lead_agent_id"],
        "lead"
    );
    root_id.to_string()
}

#[ignore = "opt-in native live Team delivery; requires ASTRA_TUI_LIVE_API_URL, ASTRA_TUI_LIVE_MODEL, and ASTRA_TUI_LIVE_ACCESS_TOKEN"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_team_delivers_dependent_member_results_and_reworks_after_client_restart() {
    let _journey = pty_journey_lock().lock().await;
    let api = required_live_env(LIVE_TEAM_API_URL_ENV);
    let model = required_live_env(LIVE_TEAM_MODEL_ENV);
    let token = required_live_env(LIVE_TEAM_ACCESS_TOKEN_ENV);
    let client = reqwest::Client::builder()
        .default_headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        )]))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let team_name = format!("pty-delivery-{}", uuid::Uuid::new_v4().simple());
    let members: Vec<_> = ["lead", "builder", "reviewer"].into_iter().map(|role|
        serde_json::json!({
            "role":role, "agent_id":role, "skills":[], "mcp_servers":[],
            "system_prompt": if role == "lead" {
                "Coordinate builder then reviewer using their exact profile identities. Wait for each actual result. Never edit files yourself. Ask reviewer to independently read source CSV and the generated report."
            } else { "Carry out the delegated file task using actual tools, then report the observed result." },
            "allow_tools": if role == "lead" { vec!["agent", "tool_search", "introspect", "read_file", "write_file"] }
                else { vec!["read_file", "write_file", "tool_search"] },
            "initial_turns":6, "max_turns": if role == "lead" {24} else {12},
            "can_delegate": role == "lead", "max_delegation_depth": if role == "lead" {1} else {0}
        })
    ).collect();
    let team = live_team_json(
        &client,
        &api,
        reqwest::Method::POST,
        "/teams",
        Some(
            serde_json::json!({"name":team_name, "description":"Dependent CSV delivery",
            "members":members,"context":{}}),
        ),
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    astra_credentials::CredentialStore::with_path(home.path().join(".astra/credentials.json"))
        .mutate(|credentials| {
            credentials.profiles.insert(
                "pty-journey".into(),
                astra_credentials::Profile {
                    account_id: Some(team["user_id"].as_str().unwrap().to_string()),
                    access_token: Some(token.clone()),
                    ..Default::default()
                },
            );
        })
        .unwrap();

    std::fs::write(
        home.path().join("source.csv"),
        "id,value\na,2\nb,3\na,7\nc,5\n",
    )
    .unwrap();
    let mut astra = PtyAstra::spawn_with_config(home.path(), &api, &model, &token, &["--yes"]);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    let task = format!(
        "/team run {team_name} --lead-agent-id lead \"Deliver a CSV report using builder, then reviewer. Give their tasks the descriptions CSV builder and CSV reviewer. Count all source.csv data rows, including duplicate ids. Builder must read source.csv and write report.json with version=1, count and total (sum of value). After builder completes, reviewer must independently read source.csv and report.json, verify count and total, then write review.json with version=1, approved=true, count and total. Do not invent file results.\""
    );
    astra.paste_and_submit(&task, UI_TRANSITION_TIMEOUT);
    let session_id = wait_for_live_session_id(&mut astra, home.path());
    let first_root = assert_live_team_round(&mut astra, &client, &api, &session_id, &team, 1).await;
    for artifact in ["report.json", "review.json"] {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.path().join(artifact)).unwrap()).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["count"], 4);
        assert_eq!(value["total"], 17);
        if artifact == "review.json" {
            assert_eq!(value["approved"], true);
        }
    }
    // Completed children retain their transcripts across conversation switches.
    astra.select_conversation("CSV builder");
    astra.wait_for("CSV builder · Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("source.csv", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x0f]);
    astra.wait_for("Main conversation ·", UI_TRANSITION_TIMEOUT);
    astra.select_conversation("CSV builder");
    astra.wait_for("CSV builder · Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("source.csv", UI_TRANSITION_TIMEOUT);
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
    let mut astra = PtyAstra::spawn_with_config(home.path(), &api, &model, &token, &["--yes"]);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.paste_and_submit(&format!("/resume {session_id}"), UI_TRANSITION_TIMEOUT);
    astra.wait_for("Resumed", Duration::from_secs(30));
    astra.paste_and_submit("Change the duplicate rule: keep the last row for each id. Continue with the same builder and reviewer, in that order, and rewrite report.json and review.json with version=2, count and total. Reviewer must independently reread source.csv and the new report. Each member must read its existing output before rewriting it.", UI_TRANSITION_TIMEOUT);
    let second_root =
        assert_live_team_round(&mut astra, &client, &api, &session_id, &team, 2).await;
    assert_ne!(first_root, second_root);
    for artifact in ["report.json", "review.json"] {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.path().join(artifact)).unwrap()).unwrap();
        assert_eq!(value["version"], 2);
        assert_eq!(value["count"], 3);
        assert_eq!(value["total"], 15);
        if artifact == "review.json" {
            assert_eq!(value["approved"], true);
        }
    }
    let audit: astra_services::session_audit::SessionAuditSummary = serde_json::from_value(
        live_team_json(
            &client,
            &api,
            reqwest::Method::GET,
            &format!("/sessions/{session_id}/audit/summary"),
            None,
        )
        .await,
    )
    .unwrap();
    assert_eq!(audit.session_id, session_id);
    assert_eq!(audit.turn_count, 2);
    let usage = audit.request_usage;
    assert_eq!(
        usage.scope,
        astra_services::session_audit::SessionRequestUsageScope::SessionAllRuns
    );
    assert!(
        usage.request_count >= 6,
        "both leads and all four children infer"
    );
    assert_eq!(usage.nonterminal_attempt_count, 0);
    for lane in [
        usage.fresh_input_tokens,
        usage.cache_read_tokens,
        usage.cache_creation_tokens,
        usage.output_tokens,
    ] {
        assert!(lane.observed_attempts <= usage.request_count);
        assert_eq!(lane.known_tokens.is_some(), lane.observed_attempts > 0);
    }
    if let Some(amount) = audit.cost.estimated_cost_usd {
        assert!(amount.is_finite() && amount >= 0.0);
        assert!(audit.cost.unavailable_reason.is_none());
    } else {
        assert!(audit.cost.unavailable_reason.is_some());
    }
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
    live_team_json(
        &client,
        &api,
        reqwest::Method::DELETE,
        &format!("/teams/{team_name}"),
        None,
    )
    .await;
}
