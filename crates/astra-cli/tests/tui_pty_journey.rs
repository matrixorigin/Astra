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
const LIVE_API_URL_ENV: &str = "ASTRA_TUI_LIVE_API_URL";
const LIVE_MODEL_ENV: &str = "ASTRA_TUI_LIVE_MODEL";
const LIVE_MEMBER_MODEL_ENV: &str = "ASTRA_TUI_LIVE_MEMBER_MODEL";
const LIVE_ACCESS_TOKEN_ENV: &str = "ASTRA_TUI_LIVE_ACCESS_TOKEN";

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

    fn paste_and_submit(&mut self, text: &str) {
        // The journey is injecting a whole message, not simulating a human
        // typing one character at a time. Use the terminal's bracketed-paste
        // protocol so the application receives the same typed event as a
        // real terminal paste. Raw bulk bytes intentionally exercise the
        // fallback paste-burst detector, where Enter is briefly interpreted
        // as a pasted newline rather than a submit gesture.
        self.write(b"\x1b[200~");
        self.write(text.as_bytes());
        self.write(b"\x1b[201~");
        // Terminal input is ordered. Submit after the paste terminator and let
        // callers wait for the actual effect, not its possibly folded rendering.
        self.write(b"\r");
    }

    fn select_completed_conversation(&mut self, name: &str) {
        self.write(&[0x07]);
        self.wait_for("H ", UI_TRANSITION_TIMEOUT);
        if !self.current_screen().contains("H active only") {
            self.wait_for("H history", UI_TRANSITION_TIMEOUT);
            self.write(b"h");
        }
        let row = format!(". {name}");
        self.wait_for(&row, UI_TRANSITION_TIMEOUT);
        let choice = self
            .current_screen()
            .lines()
            .find_map(|line| {
                let (prefix, _) = line.split_once(&row)?;
                prefix.split_whitespace().last()?.parse::<usize>().ok()
            })
            .expect("conversation is selectable in the picker");
        self.write(choice.to_string().as_bytes());
        let selected_row = format!(" {choice}. {name} ·");
        self.wait_for_screen(&selected_row, UI_TRANSITION_TIMEOUT, |screen| {
            screen
                .lines()
                .any(|line| line.trim_start().starts_with('›') && line.contains(&selected_row))
        });
        self.write(b"\r");
    }

    fn select_conversation_run(&mut self, run_id: &str, ordinal: u8, control: Option<&str>) {
        assert!((1..=9).contains(&ordinal));
        // The renderer's selected footer exposes 17 run characters plus an
        // ellipsis. Never match identities in the parent transcript or queue
        // relative navigation ahead of the rendered selection. Number selection
        // is idempotent even when the row arrives after the picker opens.
        let identity = run_id.chars().take(17).collect::<String>();
        let selected = |screen: &str| {
            let lines: Vec<_> = screen.lines().collect();
            let header = lines.iter().rposition(|line| {
                let line = line.trim_start();
                line.starts_with("Conversations") || line.starts_with("Agent runs")
            })?;
            lines[header + 1..].iter().rev().find_map(|line| {
                let (prefix, detail) = line.split_once("run ")?;
                if prefix.chars().any(char::is_alphanumeric) {
                    return None;
                }
                let (run, _) = detail.split_once(" · parent ")?;
                if run.chars().any(char::is_whitespace) {
                    return None; // The identity-unavailable placeholder is not a selection acknowledgement.
                }
                Some(run.to_owned())
            })
        };
        let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
        self.wait_for_screen(
            "conversation picker identity",
            deadline.saturating_duration_since(Instant::now()),
            |screen| selected(screen).is_some(),
        );
        loop {
            if selected(&self.current_screen()).is_some_and(|run| run.starts_with(&identity)) {
                if let Some(control) = control {
                    self.wait_for_screen(
                        "selected conversation control",
                        deadline.saturating_duration_since(Instant::now()),
                        |screen| {
                            selected(screen).is_some_and(|run| run.starts_with(&identity))
                                && screen.contains(control)
                        },
                    );
                }
                return;
            }
            assert!(
                Instant::now() < deadline,
                "fixture missed the selectable member window; not a control pass"
            );
            self.write(&[b'0' + ordinal]);
            let next_key = Instant::now() + Duration::from_millis(25);
            while Instant::now() < next_key {
                self.receive(next_key.saturating_duration_since(Instant::now()));
            }
        }
    }

    fn signal(&self, signal: nix::sys::signal::Signal) {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.child.id() as i32), signal)
            .expect("signal Astra PTY child");
    }

    fn wait_for(&mut self, needle: &str, timeout: Duration) {
        self.wait_for_screen(needle, timeout, |screen| screen.contains(needle));
    }

    fn wait_for_screen(&mut self, needle: &str, timeout: Duration, matches: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            if matches(&self.current_screen()) {
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

fn selected_task_slot(screen: &str) -> Option<String> {
    screen.lines().find_map(|line| {
        let numbered = line.trim_start().strip_prefix('›')?.trim_start();
        let (ordinal, _) = numbered.split_once('.')?;
        ordinal.parse::<usize>().ok()?;
        numbered
            .split_once("slot ")
            .map(|(_, slot)| slot.split(" · ").next().unwrap().trim().to_string())
    })
}

fn select_task_slot(astra: &mut PtyAstra, target: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let before = selected_task_slot(&astra.current_screen());
        if before.as_deref() == Some(target) {
            return;
        }
        astra.write(b"\x1b[B");
        loop {
            astra.receive(Duration::from_millis(50));
            if selected_task_slot(&astra.current_screen()) != before {
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
async fn completed_turn_does_not_restart_when_background_shell_finishes() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::BackgroundThenQuestion,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Start the task and ask me what to do next.");
    astra.wait_for("Approval · Bash", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r");
    astra.wait_for("Which option should I choose?", UI_TRANSITION_TIMEOUT);
    astra.wait_for_absent("Working", UI_TRANSITION_TIMEOUT);

    astra.paste_and_submit("/tasks");
    astra.wait_for_screen(
        "running task in the background list",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("Background tasks")
                && screen
                    .lines()
                    .any(|line| line.contains("gated background") && line.contains("running"))
                && screen.contains("Enter details")
        },
    );
    astra.write(b"\r");
    astra.wait_for_screen(
        "controls for the running task",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("Esc list")
                && screen.contains("S stop")
                && screen.contains("gated background task")
        },
    );

    // Release the real process while its detail panel is open.
    std::fs::write(home.path().join("release-background"), "go").unwrap();
    astra.wait_for_screen(
        "retained task output and terminal state",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("Latest output")
                && screen
                    .lines()
                    .any(|line| line.trim() == "background-task-finished")
                && screen
                    .lines()
                    .any(|line| line.contains("Esc list") && !line.contains("S stop"))
        },
    );
    astra.write(b"\x1b");
    astra.wait_for_screen(
        "completed task remains selectable",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("Background tasks")
                && screen
                    .lines()
                    .any(|line| line.contains("gated background") && line.contains("completed"))
        },
    );
    astra.write(b"\x1b");
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b[200~strt tsk\x1b[201~");
    astra.wait_for_screen("fuzzy prompt history", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("> Start the task and ask me what to do next.")
            && screen.contains("History · ↑↓ select · Tab insert")
    });
    astra.write(b"\t");
    astra.wait_for(
        "│› Start the task and ask me what to do next.",
        UI_TRANSITION_TIMEOUT,
    );
    astra.write(&[0x15]);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        astra.receive(Duration::from_millis(50));
        assert_eq!(
            mock.received_requests().len(),
            1,
            "a task update started a new turn"
        );
    }

    astra.paste_and_submit("Choose the first option.");
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "the user's follow-up was not sent"
        );
        astra.receive(Duration::from_millis(50));
    }
    let requests = mock.received_requests();
    assert!(
        requests[1].to_string().contains("gated background task"),
        "the next user turn lost the background update: {}",
        requests[1]
    );
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_turn_guidance_preserves_order_and_steers_queued_turns() {
    use axum::{
        Json,
        extract::Path,
        http::{HeaderMap, StatusCode},
        routing::{get, post},
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    let _journey = pty_journey_lock().lock().await;
    let received = Arc::new(Mutex::new(Vec::<(String, String, u64)>::new()));
    let sequence = Arc::new(AtomicUsize::new(0));
    let lookups = Arc::new(AtomicUsize::new(0));
    let release_first = Arc::new(tokio::sync::Notify::new());
    let release_rejection = Arc::new(tokio::sync::Notify::new());
    let (sent_tx, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
    let routes = axum::Router::new()
        .route(
            "/chat/runs/{run_id}/intents",
            post({
                let received = received.clone();
                let sequence = sequence.clone();
                let release_first = release_first.clone();
                let release_rejection = release_rejection.clone();
                move |Path(run_id): Path<String>, headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let received = received.clone();
                    let sequence = sequence.clone();
                    let release_first = release_first.clone();
                    let release_rejection = release_rejection.clone();
                    let sent_tx = sent_tx.clone();
                    async move {
                        let n = sequence.fetch_add(1, Ordering::SeqCst);
                        let intent_id = body["intent_id"].as_str().unwrap().to_string();
                        let text = body["input"]["content"].as_str().unwrap().to_string();
                        let index = (n as u64) * 2 + 1;
                        let rejected = text == "needs correction";
                        if !rejected {
                            let mut received = received.lock().unwrap();
                            if !received.iter().any(|(id, _, _)| id == &intent_id) {
                                received.push((intent_id.clone(), text.clone(), index));
                            }
                        }
                        let bearer = headers.get("authorization").unwrap().to_str().unwrap().to_owned();
                        sent_tx.send((run_id.clone(), text.clone(), bearer)).unwrap();
                        if n == 0 {
                            release_first.notified().await;
                        }
                        if matches!(text.as_str(), "ambiguous instruction" | "discarded uncertain") {
                            (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"detail":"ack unavailable"})))
                        } else if rejected {
                            release_rejection.notified().await;
                            (StatusCode::FORBIDDEN, Json(serde_json::json!({"detail": "Please correct this message"})))
                        } else {
                            (StatusCode::OK, Json(serde_json::json!({
                                "run_id": run_id, "intent_id": intent_id,
                                "status": "accepted_remote", "duplicate": false, "event_index": index
                            })))
                        }
                    }
                }
            }),
        )
        .route(
            "/chat/runs/{run_id}/intents/{intent_id}",
            get({
                let received = received.clone();
                let lookups = lookups.clone();
                move |Path((_run_id, intent_id)): Path<(String, String)>| {
                    let received = received.clone();
                    let lookups = lookups.clone();
                    async move {
                        lookups.fetch_add(1, Ordering::SeqCst);
                        if received.lock().unwrap().iter().any(|(id, _, _)| id == &intent_id) {
                            (StatusCode::OK, Json(serde_json::json!("applied")))
                        } else {
                            (StatusCode::NOT_FOUND, Json(serde_json::json!("unknown")))
                        }
                    }
                }
            }),
        );
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration_and_routes(
        astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
        true,
        routes,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn_with_config(
        home.path(),
        &mock.base_url,
        "mock-model",
        "gateway-token",
        &[],
    );
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.paste_and_submit("Start a review.");
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().is_empty() {
        assert!(Instant::now() < deadline, "turn did not reach the server");
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    astra.paste_and_submit("first instruction");
    astra.wait_for("first instruction", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("second instruction");
    astra.wait_for("second instruction", UI_TRANSITION_TIMEOUT);
    assert!(
        sent_rx.try_recv().is_err(),
        "guidance cannot be sent before run binding"
    );
    mock.release_held_binding();
    let first = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        sent_rx.try_recv().is_err(),
        "second message overtook the delayed first acknowledgement"
    );
    // The disposition can beat the POST response. The queue must settle now
    // and must not be recreated by the later acceptance receipt.
    let (intent_id, content, event_index) = received.lock().unwrap()[0].clone();
    mock.send_held_stream_event(serde_json::json!({
        "type": "user_intent_applied", "run_id": "mock-run-agent-root",
        "intent_id": intent_id, "delivery": "guide_current_run",
        "status": "applied", "event_index": event_index, "content": content,
    }));
    astra.wait_for_screen(
        "first applied before POST ACK",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("› first instruction")
                && !screen.contains("received · first instruction")
        },
    );
    release_first.notify_one();
    let second = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first,
        (
            "mock-run-agent-root".into(),
            "first instruction".into(),
            "Bearer gateway-token".into()
        )
    );
    assert_eq!(
        second,
        (
            "mock-run-agent-root".into(),
            "second instruction".into(),
            "Bearer gateway-token".into()
        )
    );
    astra.wait_for_screen("second POST accepted", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("received · second instruction")
    });
    let (intent_id, content, event_index) = received.lock().unwrap()[1].clone();
    mock.send_held_stream_event(serde_json::json!({
        "type": "user_intent_applied", "run_id": "mock-run-agent-root",
        "intent_id": intent_id, "delivery": "guide_current_run",
        "status": "applied", "event_index": event_index, "content": content,
    }));
    astra.wait_for_screen(
        "live guidance settled before terminal",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("› first instruction")
                && screen.contains("› second instruction")
                && !screen.contains("received · first instruction")
                && !screen.contains("received · second instruction")
        },
    );
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        0,
        "live disposition must not wait for the terminal lookup"
    );
    assert_eq!(mock.received_requests().len(), 1, "the turn remains open");

    // The server admitted this input but both POST acknowledgements were
    // lost. A later stream disposition must unblock the same-turn FIFO.
    astra.paste_and_submit("ambiguous instruction");
    for _ in 0..2 {
        let sent = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sent.1, "ambiguous instruction");
    }
    astra.wait_for("Message delivery is unknown", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("later live question");
    astra.wait_for("later live question", UI_TRANSITION_TIMEOUT);
    assert!(
        sent_rx.try_recv().is_err(),
        "an unknown prior message must retain FIFO order"
    );
    let (intent_id, content, event_index) = received
        .lock()
        .unwrap()
        .iter()
        .find(|(_, text, _)| text == "ambiguous instruction")
        .unwrap()
        .clone();
    mock.send_held_stream_event(serde_json::json!({
        "type": "user_intent_applied", "run_id": "mock-run-agent-root",
        "intent_id": intent_id, "delivery": "guide_current_run",
        "status": "applied", "event_index": event_index, "content": content,
    }));
    let later = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(later.1, "later live question");
    let (intent_id, content, event_index) = received
        .lock()
        .unwrap()
        .iter()
        .find(|(_, text, _)| text == "later live question")
        .unwrap()
        .clone();
    mock.send_held_stream_event(serde_json::json!({
        "type": "user_intent_applied", "run_id": "mock-run-agent-root",
        "intent_id": intent_id, "delivery": "guide_current_run",
        "status": "applied", "event_index": event_index, "content": content,
    }));
    assert_eq!(
        mock.received_requests().len(),
        1,
        "the original turn is still running"
    );

    astra.paste_and_submit("discarded uncertain");
    for _ in 0..2 {
        let sent = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sent.1, "discarded uncertain");
    }
    astra.wait_for("Message delivery is unknown", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("after discard");
    astra.wait_for("after discard", UI_TRANSITION_TIMEOUT);
    assert!(sent_rx.try_recv().is_err());
    astra.write(b"\x1bu"); // Open the existing recovery view.
    astra.wait_for("delivery unknown", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1bd"); // Discard the local copy explicitly.
    astra.wait_for("may still apply it", UI_TRANSITION_TIMEOUT);
    let after_discard = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_discard.1, "after discard");
    let (intent_id, content, event_index) = received
        .lock()
        .unwrap()
        .iter()
        .find(|(_, text, _)| text == "after discard")
        .unwrap()
        .clone();
    mock.send_held_stream_event(serde_json::json!({
        "type": "user_intent_applied", "run_id": "mock-run-agent-root",
        "intent_id": intent_id, "delivery": "guide_current_run",
        "status": "applied", "event_index": event_index, "content": content,
    }));
    astra.paste_and_submit("needs correction");
    let rejected = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rejected.1, "needs correction");
    astra.paste_and_submit("later pending");
    astra.wait_for("later pending", UI_TRANSITION_TIMEOUT);
    assert!(
        sent_rx.try_recv().is_err(),
        "queued input must wait for the prior result"
    );
    release_rejection.notify_one();
    astra.wait_for("403", UI_TRANSITION_TIMEOUT);
    astra.wait_for_screen("both recovered messages", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("needs correction") && screen.contains("later pending")
    });
    assert!(
        sent_rx.try_recv().is_err(),
        "a rejected message must not let later input overtake it"
    );
    // Rejected input remains scoped to this session until explicitly recovered.
    astra.write(b"\x1bu");
    astra.wait_for_screen("first message recovered", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("↳ needs correction")
    });
    astra.write(&[0x03]);
    astra.wait_for_absent("↳ needs correction", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1bu");
    astra.wait_for_screen("later message recovered", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("› later pending")
    });
    astra.write(&[0x03]); // Clear the recovered draft without stopping the task.
    astra.wait_for_absent("later pending", UI_TRANSITION_TIMEOUT);
    assert!(mock.cancellation_requests().is_empty());
    astra.paste_and_submit("corrected instruction");
    let corrected = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        corrected,
        (
            "mock-run-agent-root".into(),
            "corrected instruction".into(),
            "Bearer gateway-token".into()
        )
    );
    // Future turns retain their FIFO while Enter continues to guide whichever
    // task is running, including a task that itself came from the queue.
    astra.write(b"\x1b[200~task B\x1b[201~\x11");
    astra.wait_for("task B", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b[200~task C\x1b[201~\x11");
    astra.wait_for("task C", UI_TRANSITION_TIMEOUT);
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        0,
        "an active primary stream must not start a second observer"
    );
    mock.release_held_response();
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "task B did not start\n{}",
            astra.screen_diagnostic()
        );
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    assert_eq!(mock.received_requests()[1]["message"], "task B");
    let owners = std::fs::read_dir(home.path().join(".astra/sessions/v1/users")).unwrap();
    let journal = owners
        .filter_map(Result::ok)
        .filter_map(|owner| {
            std::fs::read_to_string(owner.path().join("sessions/mock-session.jsonl")).ok()
        })
        .collect::<String>();
    let recorded = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|event| {
            event
                .pointer("/metadata/user_intents")
                .and_then(|items| items.as_array())
                .cloned()
        })
        .find(|items| {
            items
                .iter()
                .any(|item| item["content"] == "first instruction")
        })
        .expect("the first turn must include recovered guidance before the queued turn starts");
    let content: Vec<_> = recorded
        .iter()
        .filter_map(|item| item["content"].as_str())
        .collect();
    assert_eq!(
        content,
        [
            "first instruction",
            "second instruction",
            "ambiguous instruction",
            "later live question",
            "after discard",
            "corrected instruction"
        ]
    );
    astra.paste_and_submit("guide task B");
    mock.release_held_binding();
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    let guided = loop {
        if let Ok(guided) = sent_rx.try_recv() {
            break guided;
        }
        assert!(
            Instant::now() < deadline,
            "guidance did not reach task B; accepted={:?}\n{}",
            received.lock().unwrap(),
            astra.screen_diagnostic()
        );
        // Answer terminal capability/position queries while the new turn binds.
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    };
    assert_eq!(guided.1, "guide task B");
    assert_eq!(
        mock.received_requests().len(),
        2,
        "task C must remain queued"
    );

    mock.release_held_response();
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().len() < 3 {
        assert!(
            Instant::now() < deadline,
            "task C did not start\n{}",
            astra.screen_diagnostic()
        );
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    assert_eq!(mock.received_requests()[2]["message"], "task C");
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

fn seed_account(home: &std::path::Path) {
    // Exercise the authenticated account binding used by production run controls.
    astra_credentials::CredentialStore::with_path(home.join(".astra/credentials.json"))
        .mutate(|credentials| {
            credentials.profiles.insert(
                "pty-journey".into(),
                astra_credentials::Profile {
                    account_id: Some("pty-owner".into()),
                    access_token: Some("pty-journey-token".into()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_plan_guidance_can_be_recovered_without_exiting_plan_mode() {
    use axum::{Json, extract::Path, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    let _journey = pty_journey_lock().lock().await;
    for scenario in ["active", "changed-mode", "closed", "permission-failure"] {
        let closed = scenario == "closed";
        let (sent_tx, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
        let (permission_tx, mut permission_rx) = tokio::sync::mpsc::unbounded_channel();
        let submissions = Arc::new(AtomicU64::new(0));
        let revision = Arc::new(AtomicU64::new(0));
        let routes = axum::Router::new()
        .route("/chat/runs/{run_id}/intents", post(move |Path(run_id): Path<String>, Json(body): Json<serde_json::Value>| {
            let sent_tx = sent_tx.clone();
            let submissions = submissions.clone();
            async move {
                sent_tx.send(body.clone()).unwrap();
                    if submissions.fetch_add(1, Ordering::SeqCst) == 0 && scenario != "permission-failure" {
                    if closed {
                        (StatusCode::CONFLICT, Json(serde_json::json!({"error_code":"run_intent_run_terminal"})))
                    } else {
                        (StatusCode::FORBIDDEN, Json(serde_json::json!({"detail":"not admitted"})))
                    }
                } else {
                    (StatusCode::OK, Json(serde_json::json!({"run_id":run_id,
                        "intent_id":body["intent_id"], "status":"accepted_remote", "duplicate":false,
                        "event_index": 2})))
                }
            }
        }))
        .route("/chat/runs/{run_id}/permission-mode", post(move |Path(_run_id): Path<String>, Json(body): Json<serde_json::Value>| {
            let permission_tx = permission_tx.clone();
            let revision = revision.clone();
            async move {
                let revision = revision.fetch_add(1, Ordering::SeqCst) + 1;
                let mut observed = body.clone();
                observed["revision"] = revision.into();
                permission_tx.send(observed).unwrap();
                if scenario == "permission-failure" && revision == 1 {
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"detail":"could not confirm"})))
                } else {
                    (StatusCode::OK, Json(serde_json::json!({"request_id":body["request_id"], "mode":body["mode"], "revision":revision})))
                }
            }
        }));
        let mock =
            astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration_and_routes(
                astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
                true,
                routes,
            )
            .await
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        seed_trusted_workspace(home.path());
        seed_account(home.path());
        let mut astra = PtyAstra::spawn_with_config(
            home.path(),
            &mock.base_url,
            "mock-model",
            "gateway-token",
            &[],
        );
        astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
        astra.paste_and_submit("Start a task.");
        let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
        while mock.received_requests().is_empty() {
            assert!(Instant::now() < deadline);
            astra.receive(Duration::from_millis(25));
            tokio::task::yield_now().await;
        }
        mock.release_held_binding();
        astra.paste_and_submit("/plan");
        let first = tokio::time::timeout(UI_TRANSITION_TIMEOUT, permission_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first["mode"], "plan");
        if scenario == "permission-failure" {
            astra.wait_for("could not confirm", UI_TRANSITION_TIMEOUT);
            astra.paste_and_submit("continue the current task");
            let sent = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(sent["input"]["content"], "continue the current task");
            mock.send_held_stream_event(serde_json::json!({
                "type":"permission_mode_applied", "session_id":"mock-session", "run_id":"mock-run-agent-root",
                "request_id":first["request_id"], "mode":"plan", "revision":1,
                "round_index":1, "owner_generation":1
            }));
            astra.wait_for_screen(
                "late permission application",
                UI_TRANSITION_TIMEOUT,
                |screen| {
                    screen
                        .lines()
                        .any(|line| line.contains("Read-only") && !line.contains("requested"))
                },
            );
            assert!(
                sent_rx.try_recv().is_err(),
                "failed plan must not be replayed automatically"
            );
            astra.signal(nix::sys::signal::Signal::SIGHUP);
            assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
            continue;
        }
        mock.send_held_stream_event(serde_json::json!({
        "type":"permission_mode_applied", "session_id":"mock-session", "run_id":"mock-run-agent-root",
        "request_id":first["request_id"], "mode":"plan", "revision":1,
        "round_index":1, "owner_generation":1
    }));
        let rejected = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            rejected["input"]["content"]
                .as_str()
                .unwrap()
                .contains("plan")
        );
        astra.wait_for("Alt+U", UI_TRANSITION_TIMEOUT);
        if scenario == "changed-mode" {
            astra.paste_and_submit("/allow auto");
            let changed = tokio::time::timeout(UI_TRANSITION_TIMEOUT, permission_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(changed["mode"], "auto");
            mock.send_held_stream_event(serde_json::json!({
                "type":"permission_mode_applied", "session_id":"mock-session", "run_id":"mock-run-agent-root",
                "request_id":changed["request_id"], "mode":"auto", "revision":changed["revision"],
                "round_index":2, "owner_generation":1
            }));
        }
        if closed {
            mock.release_held_response();
            astra.wait_for("total ·", UI_TRANSITION_TIMEOUT);
        }
        astra.write(b"\x1bu");
        astra.wait_for_screen("recovered plan draft", UI_TRANSITION_TIMEOUT, |screen| {
            screen
                .lines()
                .any(|line| line.contains('›') && line.contains("plan"))
        });
        astra.write(b"\r");
        if closed {
            let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
            while mock.received_requests().len() < 2 {
                assert!(
                    Instant::now() < deadline,
                    "recovered plan did not start a new turn\n{}",
                    astra.screen_diagnostic()
                );
                astra.receive(Duration::from_millis(25));
                tokio::task::yield_now().await;
            }
            let requests = mock.received_requests();
            let retry = &requests[1];
            assert_eq!(retry["message"], rejected["input"]["content"]);
            assert!(
                retry["context"]["edge_tools"]
                    .as_array()
                    .is_some_and(|tools| {
                        tools
                            .iter()
                            .any(|tool| tool["function"]["name"] == "exit_plan_mode")
                    }),
                "the new turn did not expose the Plan-mode tool surface: {retry}"
            );
            astra.wait_for("Read-only", UI_TRANSITION_TIMEOUT);
            assert!(permission_rx.try_recv().is_err());
            astra.signal(nix::sys::signal::Signal::SIGHUP);
            assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
            continue;
        }
        let retried = tokio::time::timeout(UI_TRANSITION_TIMEOUT, async {
        tokio::select! {
            permission = permission_rx.recv() => {
                let permission = permission.unwrap();
                assert_eq!(permission["mode"], "plan", "retry must never exit Plan mode");
                mock.send_held_stream_event(serde_json::json!({
                    "type":"permission_mode_applied", "session_id":"mock-session", "run_id":"mock-run-agent-root",
                    "request_id":permission["request_id"], "mode":"plan", "revision":permission["revision"],
                    "round_index":2, "owner_generation":1
                }));
                sent_rx.recv().await.unwrap()
            }
            sent = sent_rx.recv() => sent.unwrap(),
        }
    }).await.unwrap();
        assert!(
            retried["input"]["content"]
                .as_str()
                .unwrap()
                .contains("plan")
        );
        assert_eq!(retried["input"]["content"], rejected["input"]["content"]);
        assert!(permission_rx.try_recv().is_err());
        astra.signal(nix::sys::signal::Signal::SIGHUP);
        assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_plan_keeps_later_guidance_behind_earlier_messages() {
    use axum::{Json, extract::Path, http::StatusCode, routing::post};
    use std::sync::Arc;
    let _journey = pty_journey_lock().lock().await;
    for uncertain in [false, true] {
        let (sent_tx, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
        let (permission_tx, mut permission_rx) = tokio::sync::mpsc::unbounded_channel();
        let permission_revision = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let routes = axum::Router::new().route("/chat/runs/{run_id}/intents", post({
            let release = release.clone();
            move |Path(run_id): Path<String>, Json(body): Json<serde_json::Value>| {
                let release = release.clone();
                let sent_tx = sent_tx.clone();
                async move {
                    sent_tx.send(body.clone()).unwrap();
                    let first_message = body["input"]["content"] == "first instruction";
                    if uncertain && first_message {
                        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({})))
                    } else {
                        if first_message {
                            release.notified().await;
                        }
                        let event_index = match body["input"]["content"].as_str() {
                            Some("first instruction") => 1,
                            Some("earlier guidance") => 3,
                            Some(text) if text.starts_with("Prepare a plan") => 5,
                            Some("continue after plan") => 9,
                            _ => 7,
                        };
                        (StatusCode::OK, Json(serde_json::json!({"run_id":run_id,
                            "intent_id":body["intent_id"], "status":"accepted_remote", "duplicate":false,
                            "event_index": event_index})))
                    }
                }
            }
        })).route("/chat/runs/{run_id}/permission-mode", post(
            move |Path(_run_id): Path<String>, Json(body): Json<serde_json::Value>| {
                let permission_tx = permission_tx.clone();
                let permission_revision = permission_revision.clone();
                async move {
                    let revision = permission_revision.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    let mut observed = body.clone();
                    observed["revision"] = revision.into();
                    permission_tx.send(observed).unwrap();
                    Json(serde_json::json!({"request_id":body["request_id"], "mode":body["mode"], "revision":revision}))
                }
            }
        ));
        let mock =
            astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration_and_routes(
                astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
                true,
                routes,
            )
            .await
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        seed_trusted_workspace(home.path());
        seed_account(home.path());
        let mut astra = PtyAstra::spawn_with_config(
            home.path(),
            &mock.base_url,
            "mock-model",
            "gateway-token",
            &[],
        );
        astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
        astra.paste_and_submit("Start a task.");
        let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
        while mock.received_requests().is_empty() {
            assert!(Instant::now() < deadline);
            astra.receive(Duration::from_millis(25));
            tokio::task::yield_now().await;
        }
        mock.release_held_binding();
        astra.paste_and_submit("first instruction");
        let first = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if uncertain {
            let retry = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(retry, first);
            astra.wait_for("Message delivery is unknown", UI_TRANSITION_TIMEOUT);
        }
        astra.paste_and_submit("earlier guidance");
        astra.wait_for("earlier guidance", UI_TRANSITION_TIMEOUT);
        assert!(sent_rx.try_recv().is_err());
        astra.paste_and_submit("/plan inspect first");
        astra.wait_for("Plan queued behind earlier input", UI_TRANSITION_TIMEOUT);
        astra.paste_and_submit("later question");
        astra.wait_for("later question", UI_TRANSITION_TIMEOUT);
        assert!(
            permission_rx.try_recv().is_err(),
            "plan mode cannot overtake earlier guidance"
        );
        release.notify_one();
        mock.send_held_stream_event(serde_json::json!({
            "type":"user_intent_applied", "run_id":"mock-run-agent-root",
            "intent_id":first["intent_id"], "delivery":"guide_current_run", "status":"applied",
            "event_index":1, "content":"first instruction",
        }));
        astra.wait_for_screen("first message applied", UI_TRANSITION_TIMEOUT, |screen| {
            screen.contains("› first instruction") && !screen.contains("delivery is unknown")
        });
        let earlier = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(earlier["input"]["content"], "earlier guidance");
        mock.send_held_stream_event(serde_json::json!({
            "type":"user_intent_applied", "run_id":"mock-run-agent-root",
            "intent_id":earlier["intent_id"], "delivery":"guide_current_run", "status":"applied",
            "event_index":3, "content":"earlier guidance",
        }));
        astra.wait_for_screen(
            "earlier guidance applied",
            UI_TRANSITION_TIMEOUT,
            |screen| screen.contains("› earlier guidance"),
        );
        let permission = tokio::time::timeout(UI_TRANSITION_TIMEOUT, permission_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(permission["mode"], "plan");
        assert!(
            sent_rx.try_recv().is_err(),
            "plan guidance preceded permission application"
        );
        mock.send_held_stream_event(serde_json::json!({
            "type":"permission_mode_applied", "session_id":"mock-session", "run_id":"mock-run-agent-root",
            "request_id":permission["request_id"], "mode":"plan", "revision":1,
            "round_index":2, "owner_generation":1
        }));
        let plan = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            plan["input"]["content"],
            "Prepare a plan for this goal before making changes: inspect first"
        );
        assert!(
            sent_rx.try_recv().is_err(),
            "later question overtook plan application"
        );
        mock.send_held_stream_event(serde_json::json!({
            "type":"user_intent_applied", "run_id":"mock-run-agent-root",
            "intent_id":plan["intent_id"], "delivery":"guide_current_run", "status":"applied",
            "event_index":5, "content":plan["input"]["content"],
        }));
        let later = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(later["input"]["content"], "later question");
        mock.send_held_stream_event(serde_json::json!({
            "type":"user_intent_applied", "run_id":"mock-run-agent-root",
            "intent_id":later["intent_id"], "delivery":"guide_current_run", "status":"applied",
            "event_index":7, "content":"later question",
        }));
        astra.wait_for_screen("later guidance applied", UI_TRANSITION_TIMEOUT, |screen| {
            screen.contains("› later question")
        });
        astra.paste_and_submit("/allow auto");
        let exit_plan = tokio::time::timeout(UI_TRANSITION_TIMEOUT, permission_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit_plan["mode"], "auto");
        astra.paste_and_submit("continue after plan");
        astra.wait_for("continue after plan", UI_TRANSITION_TIMEOUT);
        assert!(sent_rx.try_recv().is_err(), "guidance overtook plan exit");
        mock.send_held_stream_event(serde_json::json!({
            "type":"permission_mode_applied", "session_id":"mock-session", "run_id":"mock-run-agent-root",
            "request_id":exit_plan["request_id"], "mode":"auto", "revision":2,
            "round_index":3, "owner_generation":1
        }));
        let after_exit = tokio::time::timeout(UI_TRANSITION_TIMEOUT, sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_exit["input"]["content"], "continue after plan");
        mock.send_held_stream_event(serde_json::json!({
            "type":"user_intent_applied", "run_id":"mock-run-agent-root",
            "intent_id":after_exit["intent_id"], "delivery":"guide_current_run", "status":"applied",
            "event_index":9, "content":"continue after plan",
        }));
        astra.wait_for_screen(
            "plan exit guidance applied",
            UI_TRANSITION_TIMEOUT,
            |screen| screen.contains("› continue after plan"),
        );
        astra.paste_and_submit("/plan final task");
        let pending_plan = tokio::time::timeout(UI_TRANSITION_TIMEOUT, permission_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending_plan["mode"], "plan");
        astra.write(b"\x1b[200~explicit next turn\x1b[201~\x11");
        astra.wait_for("explicit next turn", UI_TRANSITION_TIMEOUT);
        astra.paste_and_submit("after queued turn");
        astra.wait_for("after queued turn", UI_TRANSITION_TIMEOUT);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            sent_rx.try_recv().is_err(),
            "guidance overtook an explicit next-turn entry"
        );
        mock.release_held_response();
        astra.wait_for(
            "The task ended before its planning request was delivered",
            UI_TRANSITION_TIMEOUT,
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            mock.received_requests().len(),
            1,
            "an undelivered plan must fence later follow-ups"
        );
        astra.signal(nix::sys::signal::Signal::SIGHUP);
        assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_model_picker_requests_next_round_without_interrupting() {
    use axum::{Json, extract::Path, routing::post};
    use std::sync::{Arc, Mutex};
    let _journey = pty_journey_lock().lock().await;
    let changes = Arc::new(Mutex::new(Vec::new()));
    let captured = changes.clone();
    let routes = axum::Router::new().route(
        "/chat/runs/{run_id}/model",
        post(
            move |Path(run_id): Path<String>, Json(body): Json<serde_json::Value>| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push((run_id, body.clone()));
                    Json(serde_json::json!({
                        "request_id": body["request_id"], "revision": 5,
                        "model": body["model"], "reasoning": body["reasoning"]
                    }))
                }
            },
        ),
    );
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration_and_routes(
        astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
        true,
        routes,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("/model");
    astra.wait_for("Select model", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b");
    astra.wait_for_absent("Select model", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Start the current task.");
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().is_empty() {
        assert!(Instant::now() < deadline, "the current task did not start");
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    // The catalog does not require the run binding, which may arrive late.
    astra.paste_and_submit("/model");
    astra.wait_for("Select model", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b");
    astra.wait_for_absent("Select model", UI_TRANSITION_TIMEOUT);
    mock.release_held_binding();
    astra.wait_for("Parent acknowledged", UI_TRANSITION_TIMEOUT);

    // Queue a message first: the new choice governs all turns not yet started.
    astra.write(b"\x1b[200~queued follow-up\x1b[201~\x11");
    astra.wait_for("queued follow-up", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("/model");
    astra.wait_for("Select model", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b");
    astra.wait_for_absent("Select model", UI_TRANSITION_TIMEOUT);
    assert_eq!(
        mock.received_requests().len(),
        1,
        "cancelling the picker must leave the run alone"
    );
    astra.paste_and_submit("/model");
    astra.wait_for("Select model", UI_TRANSITION_TIMEOUT);
    astra.write(b"gpt-5");
    astra.wait_for("gpt-5", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r");
    astra.wait_for("at the next model round", UI_TRANSITION_TIMEOUT);
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while changes.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the model change was not submitted"
        );
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    let (run_id, change) = changes.lock().unwrap()[0].clone();
    assert_eq!(run_id, "mock-run-agent-root");
    assert_eq!(change["expected_session_id"], "mock-session");
    assert_eq!(change["model"]["offering_id"], "offer-gpt-5");
    assert_eq!(
        mock.received_requests().len(),
        1,
        "choosing a model must not interrupt the current run"
    );

    mock.release_held_response();
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "queued message did not start\n{}",
            astra.screen_diagnostic()
        );
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    let requests = mock.received_requests();
    assert_eq!(requests[1]["message"], "queued follow-up");
    assert!(
        requests[1].to_string().contains("gpt-5"),
        "next turn did not use the chosen model: {}",
        requests[1]
    );
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_model_catalog_failure_is_visible_without_another_input() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration(
        astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Start the current task.");
    astra.wait_for("Parent acknowledged", UI_TRANSITION_TIMEOUT);
    mock.fail_model_catalog();
    astra.paste_and_submit("/model");
    astra.wait_for("503", UI_TRANSITION_TIMEOUT);
    assert_eq!(mock.received_requests().len(), 1);
    mock.release_held_response();
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_model_picker_does_not_finalize_a_running_shell() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::ForegroundThenQuestion,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Run the gated task.");
    astra.wait_for("Approval · Bash", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r");
    astra.wait_for("Running Bash", UI_TRANSITION_TIMEOUT);
    assert!(mock.tool_results().is_empty());
    astra.paste_and_submit("/model");
    astra.wait_for("Select model", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1b");
    astra.wait_for_absent("Select model", UI_TRANSITION_TIMEOUT);
    assert!(mock.tool_results().is_empty());
    assert!(
        !astra
            .current_screen()
            .to_ascii_lowercase()
            .contains("uncertain")
    );
    std::fs::write(home.path().join("release-background"), "go").unwrap();
    astra.wait_for("Which option should I choose?", UI_TRANSITION_TIMEOUT);
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_permission_command_shows_requested_until_execution_applies_it() {
    use axum::{Json, extract::Path, http::HeaderMap, routing::post};
    let _journey = pty_journey_lock().lock().await;
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let routes = axum::Router::new().route(
        "/chat/runs/{run_id}/permission-mode",
        post(
            move |Path(run_id): Path<String>,
                  headers: HeaderMap,
                  Json(body): Json<serde_json::Value>| {
                let requests = requests.clone();
                async move {
                    let bearer = headers
                        .get("authorization")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned();
                    requests.send((run_id, body.clone(), bearer)).unwrap();
                    Json(serde_json::json!({
                        "request_id": body["request_id"], "mode": body["mode"], "revision": 1
                    }))
                }
            },
        ),
    );
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration_and_routes(
        astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
        false,
        routes,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn_with_config(
        home.path(),
        &mock.base_url,
        "mock-model",
        "gateway-token",
        &[],
    );
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Start the current task.");
    astra.wait_for("Parent acknowledged", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("/allow read_only");
    let (run_id, body, bearer) = tokio::time::timeout(UI_TRANSITION_TIMEOUT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run_id, "mock-run-agent-root");
    assert_eq!(bearer, "Bearer gateway-token");
    assert_eq!(body["expected_session_id"], "mock-session");
    assert_eq!(body["mode"], "plan");
    astra.wait_for("requested: Read-only", UI_TRANSITION_TIMEOUT);
    assert!(
        astra.current_screen().contains("Ask"),
        "unapplied mode must not replace the live policy"
    );
    mock.release_held_response();
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_plan_waits_for_permission_and_keeps_undelivered_goal() {
    use axum::{Json, extract::Path, routing::post};
    let _journey = pty_journey_lock().lock().await;
    let (permissions, mut permission_rx) = tokio::sync::mpsc::unbounded_channel();
    let (guidance, mut guidance_rx) = tokio::sync::mpsc::unbounded_channel();
    let routes = axum::Router::new()
        .route(
            "/chat/runs/{run_id}/permission-mode",
            post(move |Path(run_id): Path<String>, Json(body): Json<serde_json::Value>| {
                let permissions = permissions.clone();
                async move {
                    permissions.send((run_id, body.clone())).unwrap();
                    Json(serde_json::json!({"request_id": body["request_id"], "mode": body["mode"], "revision": 1}))
                }
            }),
        )
        .route(
            "/chat/runs/{run_id}/intents",
            post(move |Json(body): Json<serde_json::Value>| {
                let guidance = guidance.clone();
                async move {
                    guidance.send(body.clone()).unwrap();
                    Json(serde_json::json!({"run_id":"mock-run-agent-root", "intent_id":body["intent_id"], "status":"accepted_remote", "duplicate":false, "event_index":4}))
                }
            }),
        );
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration_and_routes(
        astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
        false,
        routes,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn_with_config(
        home.path(),
        &mock.base_url,
        "mock-model",
        "gateway-token",
        &[],
    );
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Start the current task.");
    astra.wait_for("Parent acknowledged", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("/plan inspect the failure");
    let (run_id, body) = tokio::time::timeout(UI_TRANSITION_TIMEOUT, permission_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run_id, "mock-run-agent-root");
    assert_eq!(body["mode"], "plan");
    astra.wait_for("requested: Read-only", UI_TRANSITION_TIMEOUT);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), guidance_rx.recv())
            .await
            .is_err(),
        "planning guidance must not precede the permission application ACK"
    );
    mock.release_held_response();
    astra.wait_for("planning request was delivered", UI_TRANSITION_TIMEOUT);
    astra.write(b"\x1bu");
    astra.wait_for_screen("undelivered plan draft", UI_TRANSITION_TIMEOUT, |screen| {
        screen
            .lines()
            .any(|line| line.contains('›') && line.contains("/plan inspect the failure"))
    });
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

async fn check_unknown_root_message(disposition: &'static str, next_turn: bool, hold_lookup: bool) {
    use axum::{
        Json,
        extract::Path,
        http::StatusCode,
        routing::{get, post},
    };
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (checked, mut checks) = tokio::sync::mpsc::unbounded_channel();
    let release_lookup = std::sync::Arc::new(tokio::sync::Notify::new());
    let lookup_gate = release_lookup.clone();
    let routes = axum::Router::new()
        .route(
            "/chat/runs/{run_id}/intents",
            post(move |Json(body): Json<serde_json::Value>| {
                let sent = sent.clone();
                async move {
                    sent.send(body).unwrap();
                    (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({})))
                }
            }),
        )
        .route(
            "/chat/runs/{run_id}/intents/{intent_id}",
            get(move |Path((run, intent)): Path<(String, String)>| {
                let checked = checked.clone();
                let lookup_gate = lookup_gate.clone();
                async move {
                    checked.send((run, intent)).unwrap();
                    if hold_lookup {
                        lookup_gate.notified().await;
                    }
                    Json(disposition)
                }
            }),
        );
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration_and_routes(
        astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
        true,
        routes,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn_with_config(
        home.path(),
        &mock.base_url,
        "mock-model",
        "gateway-token",
        &[],
    );
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Start the current task.");
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().is_empty() {
        assert!(Instant::now() < deadline, "turn did not reach the server");
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    mock.release_held_binding();
    let message = if disposition == "applied" {
        if next_turn {
            format!("What happened?\n{}retained end", "line\n".repeat(2_000))
        } else {
            format!("What happened? {} retained end", "details-".repeat(24))
        }
    } else {
        "What happened?".into()
    };
    astra.paste_and_submit(&message);
    let first = tokio::time::timeout(UI_TRANSITION_TIMEOUT, received.recv())
        .await
        .unwrap()
        .unwrap();
    let retry = tokio::time::timeout(UI_TRANSITION_TIMEOUT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["intent_id"], retry["intent_id"]);
    astra.wait_for("Message delivery is unknown", UI_TRANSITION_TIMEOUT);
    mock.release_held_response();
    astra.wait_for("total ·", UI_TRANSITION_TIMEOUT);
    if next_turn {
        astra.paste_and_submit("Start the next task.");
        let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
        while mock.received_requests().len() < 2 {
            assert!(
                Instant::now() < deadline,
                "next task did not reach the server"
            );
            astra.receive(Duration::from_millis(25));
            tokio::task::yield_now().await;
        }
        mock.release_held_binding();
        astra.wait_for("Start the next task.", UI_TRANSITION_TIMEOUT);
    }
    astra.write(b"\x1bu");
    let (run, intent) = tokio::time::timeout(UI_TRANSITION_TIMEOUT, checks.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run, "mock-run-agent-root");
    assert_eq!(intent, first["intent_id"].as_str().unwrap());
    if hold_lookup {
        astra.wait_for("read-only", UI_TRANSITION_TIMEOUT);
        astra.write(b"\x1b[200~");
        astra.write(b"SHOULD_NOT_BE_INSERTED");
        astra.write(b"\x1b[201~");
        astra.wait_for("Paste not inserted", UI_TRANSITION_TIMEOUT);
        release_lookup.notify_one();
    }
    astra.wait_for_screen(
        "settled message remains in focus",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.lines().any(|line| {
                line.contains("main conversation")
                    && line.contains(if disposition == "returned" {
                        "not used"
                    } else {
                        "applied"
                    })
            })
        },
    );
    if hold_lookup {
        // A non-bracketed paste may deliver several key events after the
        // receipt. None may reach the composer or submit a partial message.
        astra.write(b"TRAILING_INPUT_MUST_NOT_SEND\r");
        astra.wait_for("Input not inserted", UI_TRANSITION_TIMEOUT);
    }
    astra.write(b"\x1b");
    astra.wait_for_screen("recovery view closed", UI_TRANSITION_TIMEOUT, |screen| {
        !screen
            .lines()
            .any(|line| line.contains("main conversation") && line.contains("read-only"))
            && screen.contains(if disposition == "returned" {
                "Unsent message"
            } else {
                "Earlier message applied"
            })
    });
    if hold_lookup {
        assert_eq!(
            mock.received_requests().len(),
            1,
            "trailing input must not start a new turn"
        );
        assert!(
            !astra
                .current_screen()
                .contains("TRAILING_INPUT_MUST_NOT_SEND")
        );
    }
    if disposition == "returned" {
        astra.wait_for("Earlier message was not used", UI_TRANSITION_TIMEOUT);
        astra.write(b"\x1bu");
        astra.wait_for_screen(
            "returned message in composer",
            UI_TRANSITION_TIMEOUT,
            |screen| {
                screen
                    .lines()
                    .any(|line| line.contains('›') && line.contains("What happened?"))
            },
        );
        assert!(!astra.current_screen().contains("SHOULD_NOT_BE_INSERTED"));
    } else {
        // For a quiet next turn this must appear before its stream is released.
        astra.wait_for(
            "Earlier message applied to its original task:",
            UI_TRANSITION_TIMEOUT,
        );
        let screen = astra.current_screen();
        assert!(screen.contains("What happened?"), "{screen}");
        assert!(
            !screen
                .lines()
                .any(|line| line.contains('›') && line.contains("What happened?")),
            "an old message must not become a new user turn: {screen}"
        );
        if next_turn {
            mock.release_held_response();
            astra.wait_for_screen("next turn completed", UI_TRANSITION_TIMEOUT, |screen| {
                screen.matches("total ·").count() >= 2
            });
            astra.write(b"\x12");
            astra.wait_for_screen("last user input", UI_TRANSITION_TIMEOUT, |screen| {
                screen
                    .lines()
                    .rev()
                    .find(|line| line.trim_start().starts_with('›'))
                    .is_some_and(|line| line.contains("Start the next task."))
            });
        }
        astra.write(b"\x1br");
        astra.wait_for_screen("earlier message start", UI_TRANSITION_TIMEOUT, |screen| {
            screen.contains("Earlier message") && screen.contains("What happened?")
        });
        astra.write(b"\x1b[F");
        astra.wait_for_screen("earlier message end", UI_TRANSITION_TIMEOUT, |screen| {
            screen.contains("Earlier message") && screen.contains("retained end")
        });
        astra.write(b"x");
        astra.wait_for("Input not inserted", UI_TRANSITION_TIMEOUT);
        astra.write(b"\x1b[200~SHOULD_NOT_BE_INSERTED\x1b[201~");
        astra.wait_for("Paste not inserted", UI_TRANSITION_TIMEOUT);
        assert!(!astra.current_screen().contains("SHOULD_NOT_BE_INSERTED"));
    }
    assert!(
        received.try_recv().is_err(),
        "receipt lookup must not resend guidance"
    );
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_root_message_can_be_rechecked_after_turn_completion() {
    let _journey = pty_journey_lock().lock().await;
    check_unknown_root_message("returned", false, false).await;
    check_unknown_root_message("applied", false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_root_message_is_read_only_while_receipt_is_pending() {
    let _journey = pty_journey_lock().lock().await;
    check_unknown_root_message("returned", false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_root_receipt_is_visible_during_a_quiet_next_turn() {
    let _journey = pty_journey_lock().lock().await;
    check_unknown_root_message("applied", true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guidance_releases_foreground_shell_and_reply_arrives_before_it_finishes() {
    use axum::{Json, extract::Path, routing::post};
    let _journey = pty_journey_lock().lock().await;
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let routes = axum::Router::new().route(
        "/chat/runs/{run_id}/intents",
        post(
            move |Path(run_id): Path<String>, Json(body): Json<serde_json::Value>| {
                let sent = sent.clone();
                async move {
                    sent.send(body["input"]["content"].clone()).unwrap();
                    Json(serde_json::json!({
                        "run_id": run_id, "intent_id": body["intent_id"],
                        "status": "accepted_remote", "duplicate": false, "event_index": 1
                    }))
                }
            },
        ),
    );
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_routes(
        astra_cli::cli::mock_llm::MockScenario::ForegroundThenQuestion,
        routes,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn_with_config(
        home.path(),
        &mock.base_url,
        "mock-model",
        "gateway-token",
        &[],
    );
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Run the gated task.");
    astra.wait_for("Approval · Bash", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r");
    astra.wait_for("Running Bash", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("What is the status?");
    let guidance = tokio::time::timeout(UI_TRANSITION_TIMEOUT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guidance, "What is the status?");
    astra.wait_for("Which option should I choose?", UI_TRANSITION_TIMEOUT);
    assert!(!home.path().join("release-background").exists());
    assert!(
        mock.tool_results()
            .iter()
            .any(|result| result.to_string().contains("background_task_id")),
        "the still-running shell must remain available as a managed task"
    );
    std::fs::write(home.path().join("release-background"), "go").unwrap();
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_plan_releases_only_the_current_foreground_tool_wait() {
    use axum::{Json, extract::Path, routing::post};
    let _journey = pty_journey_lock().lock().await;
    let (permissions, mut permission_rx) = tokio::sync::mpsc::unbounded_channel();
    let routes = axum::Router::new().route(
        "/chat/runs/{run_id}/permission-mode",
        post(move |Path(run_id): Path<String>, Json(body): Json<serde_json::Value>| {
            let permissions = permissions.clone();
            async move {
                permissions.send((run_id, body.clone())).unwrap();
                Json(serde_json::json!({"request_id": body["request_id"], "mode": body["mode"], "revision": 1}))
            }
        }),
    );
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_routes(
        astra_cli::cli::mock_llm::MockScenario::ForegroundThenQuestion,
        routes,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn_with_config(
        home.path(),
        &mock.base_url,
        "mock-model",
        "gateway-token",
        &[],
    );
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Run the gated task.");
    astra.wait_for("Approval · Bash", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r");
    astra.wait_for("Running Bash", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("/plan inspect the failure");
    let (_, request) = tokio::time::timeout(UI_TRANSITION_TIMEOUT, permission_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request["mode"], "plan");
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.tool_results().is_empty() {
        assert!(
            Instant::now() < deadline,
            "plan request did not release the foreground wait\n{}",
            astra.screen_diagnostic()
        );
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    assert!(
        !home.path().join("release-background").exists(),
        "the foreground process must still be running"
    );
    std::fs::write(home.path().join("release-background"), "go").unwrap();
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
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
    let mock =
        astra_cli::cli::mock_llm::MockLlmServer::start_with_held_slow_response(axum::Router::new())
            .await
            .expect("start scripted slow LLM server");
    let home = tempfile::tempdir().expect("temporary isolated Astra home");
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);

    astra.wait_for("Message Astra", Duration::from_secs(15));
    let message = format!(
        "{}\nHold this turn open.",
        "保留原文，不提交折叠标签。".repeat(100)
    );
    astra.paste_and_submit(&message);

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
    assert_eq!(mock.received_requests()[0]["message"], message);
    astra.paste_and_submit("/session");
    astra.wait_for("Session ·", UI_TRANSITION_TIMEOUT);
    astra.wait_for("session id", UI_TRANSITION_TIMEOUT);
    assert_eq!(
        mock.received_requests().len(),
        1,
        "navigation is not model input"
    );
    assert!(!astra.current_screen().contains("successfully."));
    astra.write(b"\x1b");
    astra.wait_for_absent("Session ·", UI_TRANSITION_TIMEOUT);
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

    astra.write(b"\x1b[200~/session hstr\x1b[201~");
    astra.wait_for_screen(
        "fuzzy subcommand and keyboard actions",
        UI_TRANSITION_TIMEOUT,
        |screen| screen.contains("/session history") && screen.contains("Tab insert"),
    );
    astra.write(b"\t");
    astra.wait_for("│› /session history", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x15]); // Ctrl+U clears the completed command without running it.
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);

    // A single token stays contiguous in the terminal byte stream even when
    // ratatui positions separately styled words with cursor movement codes.
    let draft = "draft_survives_transcript_round_trip";
    astra.write(draft.as_bytes());
    astra.wait_for(draft, UI_TRANSITION_TIMEOUT);

    astra.write(&[0x0f]); // Ctrl+O
    astra.wait_for("Main conversation", UI_TRANSITION_TIMEOUT);
    astra.wait_for("· Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Enter details · / search", UI_TRANSITION_TIMEOUT);

    astra.write(&[0x0f]); // Ctrl+O
    astra.wait_for(draft, UI_TRANSITION_TIMEOUT);

    astra.write(&[0x15]); // Ctrl+U clears the restored draft.
    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settled_explain_report_expands_evidence_and_prepares_a_follow_up() {
    let _journey = pty_journey_lock().lock().await;
    let mock =
        astra_cli::cli::mock_llm::MockLlmServer::start_with_held_slow_response(axum::Router::new())
            .await
            .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("/explain on");
    astra.wait_for("Explain on · report after each turn", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("Record the execution evidence.");
    astra.wait_for("Working", UI_TRANSITION_TIMEOUT);
    assert!(!astra.current_screen().contains("Explain Analyze"));

    mock.release_held_response();
    astra.wait_for(
        "Explain Analyze · Completed · 1.2s wall",
        UI_TRANSITION_TIMEOUT,
    );
    astra.wait_for("Explain Analyze HTML report ready", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("/explain report");
    astra.wait_for_screen("settled report", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("Explain · mock-run-1")
            && screen.contains("Provider answer")
            && screen.contains("Enter evidence")
    });
    astra.write(b"\r");
    astra.wait_for(
        "provider reported · in 120 · out 8 tokens",
        UI_TRANSITION_TIMEOUT,
    );
    astra.write(b"a");
    astra.wait_for(
        "About the Explain report for run mock-run-1 and turn mock-run-1-turn",
        UI_TRANSITION_TIMEOUT,
    );
    assert_eq!(
        mock.received_requests().len(),
        1,
        "asking starts with an editable draft"
    );
    astra.paste_and_submit("Which call took the longest?");
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    loop {
        let requests = mock.received_requests();
        if let Some(request) = requests.get(1) {
            let message = request["message"].as_str().expect("follow-up message");
            assert!(message.contains("mock-run-1 and turn mock-run-1-turn"));
            assert!(message.contains("Which call took the longest?"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "follow-up did not reach the server"
        );
        astra.receive(Duration::from_millis(50));
    }
    mock.release_held_response();
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
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
    astra.wait_for("Main conversation", UI_TRANSITION_TIMEOUT);
    astra.wait_for("· Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("successfully.", Duration::from_secs(10));

    astra.write(&[0x0f]);
    astra.wait_for_absent("· Transcript", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Message Astra", Duration::from_secs(10));
    astra.write(b"/exit\r");
    let status = astra.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "Astra exit status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_during_unbound_turn_reconciles_the_exact_run() {
    let _journey = pty_journey_lock().lock().await;
    let mock =
        astra_cli::cli::mock_llm::MockLlmServer::start_with_held_slow_response(axum::Router::new())
            .await
            .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.paste_and_submit("Hold this task open.");
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.received_requests().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the task did not reach the provider"
        );
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    astra.write(&[0x0f]);
    astra.wait_for("· Transcript", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x0f]);
    astra.wait_for_absent("· Transcript", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit("/exit");
    astra.wait_for_screen("exit or stopping", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("Stopping") || screen.contains("Exiting")
    });
    mock.release_held_response();
    assert!(astra.wait_for_exit(Duration::from_secs(15)).success());
    assert_eq!(mock.cancellation_requests(), ["mock-run-1"]);
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
    astra.wait_for("Main conversation", UI_TRANSITION_TIMEOUT);
    astra.wait_for("· Transcript", UI_TRANSITION_TIMEOUT);
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
    astra.wait_for("Main conversation", UI_TRANSITION_TIMEOUT);
    astra.wait_for("· Transcript", UI_TRANSITION_TIMEOUT);
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

fn live_agent_client(api: &str, token: &str) -> reqwest::Client {
    astra_core::net::client_builder_for_target(api)
        .default_headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        )]))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

fn seed_live_agent_account(home: &std::path::Path, user_id: &str, token: &str) {
    seed_trusted_workspace(home);
    astra_credentials::CredentialStore::with_path(home.join(".astra/credentials.json"))
        .mutate(|credentials| {
            credentials.profiles.insert(
                "pty-journey".into(),
                astra_credentials::Profile {
                    account_id: Some(user_id.into()),
                    access_token: Some(token.into()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
}

async fn live_agent_json(
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

async fn live_agent_run_tree(
    client: &reqwest::Client,
    api: &str,
    session_id: &str,
) -> serde_json::Value {
    let tree = live_agent_json(
        client,
        api,
        reqwest::Method::GET,
        &format!("/sessions/{session_id}/runs"),
        None,
    )
    .await;
    assert_eq!(tree["truncated"], false);
    for root in tree["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["depth"] == 0)
    {
        use astra_thin_client::SessionRunLifecycleStatus as Status;
        let status: Status = serde_json::from_value(root["status"].clone()).unwrap();
        if matches!(
            status,
            Status::Failed | Status::Cancelled | Status::Interrupted
        ) {
            let kind = root["error_code"]
                .as_str()
                .and_then(astra_core::ErrorKind::parse_tag)
                .unwrap_or(astra_core::ErrorKind::Unknown);
            panic!("live root ended {status:?}: {kind}; execution evidence retained");
        }
    }
    tree
}

#[tokio::test]
async fn live_agent_poll_stops_on_terminal_failure_without_exposing_provider_text() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let server = MockServer::start().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (status, code, failure) in [
        ("running", "tool_timeout", None),
        ("waiting", "tool_timeout", None),
        ("paused", "tool_timeout", None),
        ("completed", "tool_timeout", None),
        (
            "failed",
            "payment_required",
            Some("Failed: payment_required"),
        ),
        ("cancelled", "cancelled", Some("Cancelled: cancelled")),
        (
            "interrupted",
            "contract_violation",
            Some("Interrupted: contract_violation"),
        ),
        ("failed", "private-provider-detail", Some("Failed: unknown")),
    ] {
        let response = serde_json::json!({"truncated":false,"runs":[
            {"depth":0,"status":status,"error_code":code,"error_message":"private-provider-detail"},
            {"depth":1,"status":"failed","error_code":"tool_timeout"}
        ]});
        let _mock = Mock::given(method("GET"))
            .and(path("/sessions/fixture-session/runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response.clone()))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let client = client.clone();
        let api = server.uri();
        let result =
            tokio::spawn(
                async move { live_agent_run_tree(&client, &api, "fixture-session").await },
            )
            .await;
        match failure {
            None => assert_eq!(result.unwrap(), response),
            Some(cause) => {
                let panic = result.unwrap_err().into_panic();
                let message = panic.downcast_ref::<String>().unwrap();
                assert!(message.contains(cause), "{message}");
                assert!(!message.contains("private-provider-detail"), "{message}");
            }
        }
    }
}

async fn wait_for_live_tui_session(
    astra: &mut PtyAstra,
    client: &reqwest::Client,
    api: &str,
    existing: &std::collections::BTreeSet<&str>,
    deadline: Instant,
) -> String {
    loop {
        astra.receive(Duration::from_millis(25));
        let sessions = live_agent_json(
            client,
            api,
            reqwest::Method::GET,
            "/sessions?limit=200",
            None,
        )
        .await;
        assert!(sessions["next_cursor"].is_null());
        let fresh: Vec<_> = sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|session| session["session_id"].as_str())
            .filter(|id| !existing.contains(id))
            .collect();
        assert!(
            fresh.len() <= 1,
            "concurrent fixture sessions cannot establish this TUI's ownership"
        );
        if let Some(id) = fresh.first() {
            return (*id).to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "TUI admission did not create its session"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn live_tool_json(value: &serde_json::Value) -> serde_json::Value {
    match value.as_str() {
        Some(text) => serde_json::from_str(text).expect("structured tool arguments"),
        None => value.clone(),
    }
}

fn assert_live_work_artifacts(workspace: &std::path::Path, version: u64) {
    let master: serde_json::Value =
        serde_json::from_slice(&std::fs::read(workspace.join("customer_master.json")).unwrap())
            .unwrap();
    assert_eq!(
        master,
        serde_json::json!({"version":version,"customers":[
            {"id":"C001","name":if version == 1 {"Alice"} else {"Alice Updated"},"credit_limit":if version == 1 {100} else {150}},
            {"id":"C002","name":"Bob","credit_limit":200},
            {"id":"C003","name":"Cara","credit_limit":50}
        ]})
    );
    let exceptions: serde_json::Value =
        serde_json::from_slice(&std::fs::read(workspace.join("invoice_exceptions.json")).unwrap())
            .unwrap();
    let mut expected = vec![
        serde_json::json!({"invoice_id":"I003","customer_id":"C999","amount":30,"reason":"unknown_customer"}),
        serde_json::json!({"invoice_id":"I004","customer_id":"C003","amount":70,"reason":"over_credit_limit"}),
    ];
    if version == 1 {
        expected.insert(0,serde_json::json!({"invoice_id":"I001","customer_id":"C001","amount":120,"reason":"over_credit_limit"}));
    }
    assert_eq!(
        exceptions,
        serde_json::json!({"version":version,"exceptions":expected,"total_amount":if version == 1 {220} else {100}})
    );
}

struct LiveAgentWorkRound {
    root_id: String,
    graph: serde_json::Value,
    proposal: Option<serde_json::Value>,
}

async fn assert_live_agent_work_round(
    astra: &mut PtyAstra,
    client: &reqwest::Client,
    api: &str,
    session_id: &str,
    round: usize,
) -> LiveAgentWorkRound {
    let deadline = Instant::now() + Duration::from_secs(180);
    let tree = loop {
        astra.receive(Duration::from_millis(25));
        let tree = live_agent_run_tree(client, api, session_id).await;
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
            "live root did not deliver; execution evidence retained"
        );
        assert!(
            Instant::now() < deadline,
            "live agent turn did not settle; execution evidence retained"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
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
    assert!(
        !children.is_empty(),
        "artifact production must be delegated"
    );
    let root_projection = live_agent_json(
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
    let mut artifact_runs = std::collections::BTreeMap::new();
    let mut observed_children = std::collections::BTreeSet::new();
    for spawn in root_events
        .iter()
        .filter(|event| event["type"] == "agent_spawned")
    {
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
            observed_children.insert(child_id.to_string()),
            "a physical child has exactly one spawn event"
        );
        let projection = live_agent_json(
            client,
            api,
            reqwest::Method::GET,
            &format!("/chat/runs/{child_id}/projection?recent_limit=500"),
            None,
        )
        .await;
        assert!(projection["run_event_high_watermark"].as_i64().unwrap() < 500);
        let events = projection["recent_events"].as_array().unwrap();
        let file_call = |tool: &str, path: &str| {
            events
                .iter()
                .filter(|event| {
                    event["type"] == "tool_request"
                        && event["run_id"] == child_id
                        && event["tool"] == tool
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
                                && event["success"] == true
                                && event["transport"] == "edge_ledger"
                        })
                        .map(|end| (request, end))
                })
        };
        // Successful artifact writes identify each ordinary child producer,
        // including corrective runs.
        let artifacts: Vec<_> = ["customer_master.json", "invoice_exceptions.json"]
            .into_iter()
            .filter(|path| file_call("write_file", path).is_some())
            .collect();
        assert_eq!(
            artifacts.len(),
            1,
            "each child must produce its own artifact"
        );
        let artifact = artifacts[0];
        artifact_runs
            .entry(artifact)
            .or_insert_with(Vec::new)
            .push(child_id.to_string());
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
                "{artifact} must replay its own terminal {kind:?} facts"
            );
        }
        // The artifact receipt above proves this ordinary child performed the
        // write. The final workspace assertion verifies its contents. Do not
        // prescribe a particular read tool: equivalent workspace access paths
        // are valid, and the resulting artifact is the behavior under test.
    }
    assert_eq!(
        observed_children.len(),
        children.len(),
        "every durable child must have an observed spawn"
    );
    assert_eq!(
        artifact_runs.keys().copied().collect::<Vec<_>>(),
        ["customer_master.json", "invoice_exceptions.json"]
    );
    let latest_master = artifact_runs["customer_master.json"].last().unwrap();
    let latest_exceptions = artifact_runs["invoice_exceptions.json"].last().unwrap();
    // Check every physical invoice producer, not just the final correction. An early
    // invoice run cannot be legitimized by a later correctly ordered execution.
    for exceptions_run in &artifact_runs["invoice_exceptions.json"] {
        let started = root_events
            .iter()
            .position(|event| {
                event["type"] == "agent_spawned" && event["run_id"] == *exceptions_run
            })
            .unwrap();
        assert!(
            root_events[..started].iter().any(|event| {
                event["type"] == "agent_completed"
                    && artifact_runs["customer_master.json"]
                        .iter()
                        .any(|master| event["run_id"] == *master)
            }),
            "every invoice producer must observe a completed customer master before launch"
        );
    }
    // The successful child write receipts above establish artifact production.
    // Shell calls can also inspect and independently verify those artifacts;
    // rejecting every shell invocation would reject the requested verification.
    assert!(
        root_events
            .iter()
            .filter(|event| event["type"] == "tool_request" && event["run_id"] == root_id)
            .all(|event| event["tool"] != "write_file"),
        "parent must not replace the delegated writers with its own write_file"
    );
    let master_done = root_events
        .iter()
        .position(|event| event["type"] == "agent_completed" && event["run_id"] == *latest_master)
        .expect("observed customer-master completion");
    let exceptions_started = root_events
        .iter()
        .position(|event| event["type"] == "agent_spawned" && event["run_id"] == *latest_exceptions)
        .unwrap();
    assert!(
        master_done < exceptions_started,
        "invoice producer consumes a settled customer-master result"
    );
    let receipts = |tool: &str| -> Vec<(usize, serde_json::Value)> {
        root_events
            .iter()
            .enumerate()
            .filter_map(|(index, end)| {
                if end["type"] != "tool_call_end"
                    || end["success"] != true
                    || end["transport"] != "server_local"
                {
                    return None;
                }
                if end["tool"] != tool {
                    return None;
                }
                let (start_index, _) = root_events
                    .iter()
                    .enumerate()
                    .find(|(_, event)| {
                        event["type"] == "tool_transport_started"
                            && event["call_id"] == end["call_id"]
                            && event["tool"] == tool
                            && event["run_id"] == root_id
                            && event["transport"] == "server_local"
                    })
                    .expect("Server Work terminal has its exact start");
                assert!(start_index < index);
                assert_ne!(end["result_truncated"], serde_json::json!(true));
                let receipt: serde_json::Value = serde_json::from_str(
                    end["result"]
                        .as_str()
                        .expect("Server Work receipt is JSON text"),
                )
                .expect("Work receipt JSON");
                assert!(receipt.is_object());
                Some((index, receipt))
            })
            .collect()
    };
    let starts = receipts("start_work");
    let proposals = receipts("propose_work_plan");
    let (assignment_index, assignment) = if round == 1 {
        assert_eq!(starts.len(), 1);
        let (index, start) = &starts[0];
        assert_eq!(start["status"], "started");
        assert_eq!(start["initial_item_count"], 2);
        assert!(proposals.is_empty());
        (*index, start["initial_task"].clone())
    } else {
        assert!(starts.is_empty(), "guidance must retain the existing Work");
        let inspections = receipts("inspect_work_plan");
        let assignments = receipts("run_next_work_item");
        assert_eq!(proposals.len(), 1);
        assert_eq!(assignments.len(), 1);
        assert!(inspections.iter().any(|(index, _)| *index < proposals[0].0));
        assert_eq!(proposals[0].1["status"], "accepted");
        assert!(proposals[0].0 < assignments[0].0);
        assignments[0].clone()
    };
    let settlements = receipts("settle_work_item");
    assert_eq!(settlements.len(), 2);
    let assignments = [assignment, settlements[0].1["next_task"].clone()];
    for key in ["item_id", "attempt_id"] {
        assert_ne!(assignments[0][key], assignments[1][key]);
    }
    for ((_, settlement), assignment) in settlements.iter().zip(&assignments) {
        assert_eq!(assignment["status"], "assigned");
        assert_eq!(assignment["execution"], "primary_session");
        assert_eq!(settlement["status"], "recorded");
        assert_eq!(settlement["outcome"], "delivered");
        assert_eq!(settlement["status_scope"], "task_graph_execution");
        for key in ["item_id", "item_revision", "attempt_id"] {
            assert!(!assignment[key].is_null());
            assert_eq!(settlement[key], assignment[key]);
        }
    }
    assert!(settlements[1].1["next_task"].is_null());
    assert_eq!(settlements[1].1["next_action"], "synthesize_final_response");
    let master_started = root_events
        .iter()
        .position(|event| event["type"] == "agent_spawned" && event["run_id"] == *latest_master)
        .unwrap();
    let exceptions_done = root_events
        .iter()
        .position(|event| {
            event["type"] == "agent_completed" && event["run_id"] == *latest_exceptions
        })
        .unwrap();
    assert!(assignment_index < master_started);
    assert!(master_done < settlements[0].0 && settlements[0].0 < exceptions_started);
    assert!(exceptions_done < settlements[1].0);
    let spawns: Vec<_> = root_events
        .iter()
        .filter(|event| {
            event["type"] == "tool_call_end"
                && event["tool"] == "agent"
                && event["success"] == true
                && live_tool_json(&event["arguments"])["action"] == "spawn"
        })
        .collect();
    assert_eq!(
        spawns.len(),
        children.len(),
        "each physical child has a successful launch receipt"
    );
    let mut receipt_children = std::collections::BTreeSet::new();
    for spawn in spawns {
        let receipt = live_tool_json(&spawn["result"]);
        let child_id = receipt["run_id"]
            .as_str()
            .expect("launch receipt run identity");
        let child = children
            .iter()
            .find(|child| child["run_id"] == child_id)
            .expect("launch receipt must identify a durable child of this root");
        assert_eq!(receipt["parent_run_id"], root_id);
        assert_eq!(receipt["agent_id"], child["agent_id"]);
        assert!(
            receipt_children.insert(child_id.to_string()),
            "duplicate launch receipt child"
        );
        assert!(
            live_tool_json(&spawn["arguments"])
                .get("work_item")
                .is_none(),
            "helpers must not create a parallel Work attempt owner"
        );
    }
    assert_eq!(receipt_children, observed_children);
    let work_id = settlements[0].1["work_id"].as_str().unwrap();
    let branch_id = settlements[0].1["branch_id"].as_str().unwrap();
    let graph: serde_json::Value = client
        .get(format!(
            "{api}/v1/works/{work_id}/branches/{branch_id}/task-graph?item_limit=8&dependency_limit=8"
        ))
        .header("x-astra-work-api-major", "1")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(graph["schema_version"], 2);
    assert_eq!(graph["scope"], "declared_work");
    assert!(graph["next_cursor"].is_null());
    assert_eq!(graph["basis"]["work_id"], work_id);
    assert_eq!(graph["basis"]["branch_id"], branch_id);
    assert_eq!(graph["items"]["total"], 3);
    let entries = graph["items"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries
            .iter()
            .filter(|item| item["kind"] == "milestone")
            .count(),
        1
    );
    let items: Vec<_> = entries
        .iter()
        .filter(|item| item["kind"] == "task")
        .collect();
    assert_eq!(items.len(), 2);
    for assignment in &assignments {
        let item = items
            .iter()
            .find(|item| item["item_id"] == assignment["item_id"])
            .unwrap();
        assert_eq!(item["revision"], assignment["item_revision"]);
        assert_eq!(item["kind"], "task");
        assert_eq!(item["declaration_state"], "active");
        assert_eq!(item["execution"]["status"], "completed");
        assert_eq!(item["execution"]["terminal"], true);
        assert_eq!(item["execution"]["run"]["run_id"], root_id);
        assert_eq!(
            item["execution"]["run"]["attempt_id"],
            assignment["attempt_id"]
        );
        assert_eq!(
            item["execution"]["run"]["graph_revision"],
            graph["basis"]["graph_revision"]
        );
        assert_eq!(item["delivery"]["status"], "delivered");
    }
    for (_, settlement) in &settlements {
        assert_eq!(settlement["work_id"], work_id);
        assert_eq!(settlement["branch_id"], branch_id);
    }
    if let Some((_, start)) = starts.first() {
        assert_eq!(start["work_id"], work_id);
        assert_eq!(start["branch_id"], branch_id);
        assert_eq!(start["graph_revision"], graph["basis"]["graph_revision"]);
    }
    if let Some((_, proposal)) = proposals.first() {
        assert!(!proposal["proposal_id"].as_str().unwrap().is_empty());
        assert!(!proposal["payload_hash"].as_str().unwrap().is_empty());
        assert_eq!(
            proposal["result_graph_revision"],
            graph["basis"]["graph_revision"]
        );
        assert_eq!(
            proposal["result_branch_revision"],
            graph["basis"]["branch_revision"]
        );
        for key in ["added_items", "dependencies_added", "dependencies_removed"] {
            assert!(
                proposal["applied_mutations"][key]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        let revised = proposal["applied_mutations"]["revised_items"]
            .as_array()
            .unwrap();
        assert!(
            revised.iter().all(|revision| entries
                .iter()
                .any(|item| { item["item_id"] == revision["item_id"] })),
            "revision must belong to the existing declared graph"
        );
        let revised_tasks: std::collections::BTreeSet<_> = revised
            .iter()
            .filter(|revision| {
                items
                    .iter()
                    .any(|item| item["item_id"] == revision["item_id"])
            })
            .map(|revision| revision["item_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            revised_tasks,
            items
                .iter()
                .map(|item| item["item_id"].as_str().unwrap())
                .collect(),
            "both dependent deliverables must be revised; milestone revisions are independent"
        );
    }
    assert_eq!(graph["dependencies"]["total"], 1);
    let edges = graph["dependencies"]["entries"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["predecessor_item_id"], assignments[0]["item_id"]);
    assert_eq!(edges[0]["successor_item_id"], assignments[1]["item_id"]);
    assert_eq!(edges[0]["kind"], "dependency");
    LiveAgentWorkRound {
        root_id: root_id.to_string(),
        graph,
        proposal: proposals.first().map(|(_, receipt)| receipt.clone()),
    }
}

#[ignore = "opt-in real multi-agent Work delivery; requires ASTRA_TUI_LIVE_API_URL, ASTRA_TUI_LIVE_MODEL, and ASTRA_TUI_LIVE_ACCESS_TOKEN"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_agent_delivers_dependent_work_items_and_reworks_after_client_restart() {
    let _journey = pty_journey_lock().lock().await;
    let api = required_live_env(LIVE_API_URL_ENV);
    let model = required_live_env(LIVE_MODEL_ENV);
    let token = required_live_env(LIVE_ACCESS_TOKEN_ENV);
    let client = live_agent_client(&api, &token);
    let owner = live_agent_json(&client, &api, reqwest::Method::GET, "/auth/me", None).await;
    let home = tempfile::tempdir().unwrap();
    seed_live_agent_account(home.path(), owner["user_id"].as_str().unwrap(), &token);

    std::fs::write(
        home.path().join("customers.csv"),
        "id,name,credit_limit\nC001,Alice,100\nC002,Bob,200\nC001,Alice Updated,150\nC003,Cara,50\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("invoices.csv"),
        "invoice_id,customer_id,amount\nI001,C001,120\nI002,C002,80\nI003,C999,30\nI004,C003,70\n",
    )
    .unwrap();
    let before = live_agent_json(
        &client,
        &api,
        reqwest::Method::GET,
        "/sessions?limit=200",
        None,
    )
    .await;
    assert!(before["next_cursor"].is_null());
    let existing: std::collections::BTreeSet<_> = before["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap())
        .collect();
    let mut astra = PtyAstra::spawn_with_config(home.path(), &api, &model, &token, &["--yes"]);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.paste_and_submit(
        "Please track these two deliverables and their dependency as durable Work so we can revise them later. Delegate each artifact to a separate child with file read/write access; coordinate and verify their actual results without producing the files yourself. The first child must read customers.csv and write customer_master.json with version=1 and customers sorted by id, each containing the original id, name and numeric credit_limit. Keep the first row for each customer id. After that child completes, have a separate child independently read invoices.csv and the completed customer_master.json and write invoice_exceptions.json with version=1, exceptions sorted by invoice_id, and total_amount summing exceptions. Each exception contains invoice_id, customer_id, numeric amount and reason: unknown_customer for missing customer or over_credit_limit for amount strictly above the limit. Do not report ordinary invoices.",
    );
    let session_id = wait_for_live_tui_session(
        &mut astra,
        &client,
        &api,
        &existing,
        Instant::now() + Duration::from_secs(180),
    )
    .await;
    let first_root = assert_live_agent_work_round(&mut astra, &client, &api, &session_id, 1).await;
    assert_live_work_artifacts(home.path(), 1);
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
    let mut astra = PtyAstra::spawn_with_config(home.path(), &api, &model, &token, &["--yes"]);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.paste_and_submit(&format!("/resume {session_id}"));
    astra.wait_for("Resumed", Duration::from_secs(30));
    astra.paste_and_submit("Change the duplicate rule to keep the last customer row. Please update customer_master.json and recompute invoice_exceptions.json against the revised credit limits, keeping the same schemas and using version=2. Revise the same two tracked deliverables in place and retain their dependency. Again delegate each artifact to a separate file-writing child, starting the invoice child only after the customer-master child completes; verify their actual results before reporting delivery.");
    let second_root = assert_live_agent_work_round(&mut astra, &client, &api, &session_id, 2).await;
    assert_ne!(first_root.root_id, second_root.root_id);
    for key in ["work_id", "branch_id"] {
        assert_eq!(
            first_root.graph["basis"][key],
            second_root.graph["basis"][key]
        );
    }
    for key in ["graph_revision", "branch_revision"] {
        assert!(
            second_root.graph["basis"][key].as_i64().unwrap()
                > first_root.graph["basis"][key].as_i64().unwrap()
        );
    }
    assert!(first_root.proposal.is_none());
    let revisions = second_root.proposal.as_ref().unwrap()["applied_mutations"]["revised_items"]
        .as_array()
        .unwrap();
    for first in first_root.graph["items"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "task")
    {
        let second = second_root.graph["items"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["item_id"] == first["item_id"])
            .unwrap();
        let revision = revisions
            .iter()
            .find(|revision| revision["item_id"] == first["item_id"])
            .unwrap();
        assert_eq!(revision["from_revision"], first["revision"]);
        assert_eq!(revision["declaration_state"], "active");
        assert!(second["revision"].as_i64().unwrap() > first["revision"].as_i64().unwrap());
        assert_ne!(
            second["execution"]["run"]["attempt_id"],
            first["execution"]["run"]["attempt_id"]
        );
    }
    assert_live_work_artifacts(home.path(), 2);
    let audit: astra_services::session_audit::SessionAuditSummary = serde_json::from_value(
        live_agent_json(
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
        "both roots and all four children infer"
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
}

#[ignore = "opt-in real member controls; requires the four ASTRA_TUI_LIVE_* settings"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_child_pause_guidance_resume_preserves_execution_identity() {
    let _journey = pty_journey_lock().lock().await;
    let api = required_live_env(LIVE_API_URL_ENV);
    let model = required_live_env(LIVE_MODEL_ENV);
    let member_model = required_live_env(LIVE_MEMBER_MODEL_ENV);
    let token = required_live_env(LIVE_ACCESS_TOKEN_ENV);
    let client = live_agent_client(&api, &token);
    let catalog = live_agent_json(
        &client,
        &api,
        reqwest::Method::GET,
        "/models?limit=200",
        None,
    )
    .await;
    assert!(catalog["next_cursor"].is_null());
    let offerings: Vec<_> = catalog["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["name"] == member_model && item["is_active"] == true)
        .collect();
    assert_eq!(offerings.len(), 1);
    let offering = offerings[0]["offering_id"].as_str().unwrap();
    let owner = live_agent_json(&client, &api, reqwest::Method::GET, "/auth/me", None).await;
    let home = tempfile::tempdir().unwrap();
    seed_live_agent_account(home.path(), owner["user_id"].as_str().unwrap(), &token);
    std::fs::write(
        home.path().join("draft.txt"),
        "Release code: alpha. Audience: internal.\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("final.txt"),
        "Release code: beta. Audience: public.\n",
    )
    .unwrap();
    let before = live_agent_json(
        &client,
        &api,
        reqwest::Method::GET,
        "/sessions?limit=200",
        None,
    )
    .await;
    assert!(before["next_cursor"].is_null());
    let existing: std::collections::BTreeSet<_> = before["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap())
        .collect();
    let mut astra = PtyAstra::spawn_with_config(home.path(), &api, &model, &token, &["--yes"]);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit(&format!(
        "使用 {member_model} 比较 draft.txt 和 final.txt，告诉我正式版的发布代码，只输出代码。"
    ));
    let deadline = Instant::now() + Duration::from_secs(180);
    let session_id =
        wait_for_live_tui_session(&mut astra, &client, &api, &existing, deadline).await;
    let child = loop {
        astra.receive(Duration::from_millis(25));
        let tree = live_agent_run_tree(&client, &api, &session_id).await;
        let runs = tree["runs"].as_array().unwrap();
        if let Some(child) = runs.iter().find(|run| {
            run["depth"] == 1
                && run["status"] == "running"
                && run["available_actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|action| action == "pause")
        }) {
            // Freeze the real run at the authoritative API boundary as soon
            // as it becomes controllable. Model latency is variable; the PTY
            // journey below then exercises stable pause observation, guidance,
            // and TUI resume without racing a short child to completion.
            let child_id = child["run_id"].as_str().unwrap();
            let paused = live_agent_json(
                &client,
                &api,
                reqwest::Method::POST,
                &format!("/chat/runs/{child_id}/pause"),
                None,
            )
            .await;
            assert_eq!(paused["run_id"], child_id);
            assert_eq!(paused["status"], "paused");
            assert_eq!(paused["disposition"], "applied");
            break child.clone();
        }
        assert!(
            !runs
                .iter()
                .any(|run| run["depth"] == 0 && run["status"] == "completed"),
            "fixture missed the active member control window; not a control pass"
        );
        assert!(
            Instant::now() < deadline,
            "fixture never established an active resumable member"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let child_id = child["run_id"].as_str().unwrap();
    assert_eq!(child["runtime"]["offering_id"], offering);
    assert_eq!(child["runtime"]["model_name"], member_model);
    astra.write(&[0x07]);
    // Display names can be the task description rather than the profile role.
    // Navigate the real picker and verify its selected-run detail instead.
    loop {
        astra.receive(Duration::from_millis(25));
        let tree = live_agent_run_tree(&client, &api, &session_id).await;
        let current = tree["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|run| run["run_id"] == child_id)
            .unwrap();
        if current["status"] == "paused" {
            let parent = tree["runs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|run| run["run_id"] == child["parent_run_id"])
                .unwrap();
            assert!(
                matches!(parent["status"].as_str(), Some("running" | "waiting")),
                "a dependency pause must retain its parent's live execution"
            );
            assert!(
                current["available_actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|action| action == "resume"),
                "ContinueSession is not restoration of this execution owner"
            );
            assert!(
                !current["available_actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|action| action == "continue_session")
            );
            break;
        }
        assert!(
            current["status"] != "completed",
            "fixture missed pause admission"
        );
        assert!(Instant::now() < deadline, "member did not pause");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    astra.select_conversation_run(child_id, 2, Some("P pause/resume/continue"));
    astra.wait_for("P pause/resume/continue", UI_TRANSITION_TIMEOUT);
    let value = format!("release-{}", uuid::Uuid::new_v4().simple());
    let guidance = "正式版刚更新了，请重新读取 final.txt；最终把发布代码转成大写，只输出代码。";
    astra.write(b"g");
    astra.wait_for("What would you like this agent", UI_TRANSITION_TIMEOUT);
    astra.paste_and_submit(guidance);
    astra.write(&[0x07]);
    astra.select_conversation_run(child_id, 2, Some("P pause/resume/continue"));
    astra.write(b"\r");
    astra.wait_for_screen(
        "initial member history loaded",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("Transcript")
                && !screen.contains("Syncing durable agent history")
                && !screen.contains("Loading durable conversation")
        },
    );
    assert!(
        !astra
            .current_screen()
            .contains("Could not sync durable agent history")
    );
    assert!(!astra.current_screen().contains(&value));
    std::fs::write(
        home.path().join("final.txt"),
        format!("Release code: {value}. Audience: public.\n"),
    )
    .unwrap();
    astra.write(&[0x07]);
    astra.select_conversation_run(child_id, 2, Some("P pause/resume/continue"));
    astra.write(b"p");
    astra.write(b"\x1b");
    // Stay on this member page: no refresh or reopening may substitute for live delivery.
    astra.wait_for(
        &value.to_uppercase(),
        deadline.saturating_duration_since(Instant::now()),
    );
    let root_id = child["parent_run_id"].as_str().unwrap();
    loop {
        astra.receive(Duration::from_millis(25));
        let tree = live_agent_run_tree(&client, &api, &session_id).await;
        let runs = tree["runs"].as_array().unwrap();
        assert_eq!(runs.iter().filter(|run| run["depth"] == 1).count(), 1);
        let root = runs.iter().find(|run| run["run_id"] == root_id).unwrap();
        if root["status"] == "completed" {
            assert!(
                runs.iter()
                    .any(|run| run["run_id"] == child_id && run["status"] == "completed")
            );
            break;
        }
        assert!(
            !matches!(root["status"].as_str(), Some("failed" | "cancelled")),
            "parent failed to adopt the resumed result"
        );
        assert!(
            Instant::now() < deadline,
            "parent did not settle the actual member result"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let projection = live_agent_json(
        &client,
        &api,
        reqwest::Method::GET,
        &format!("/chat/runs/{child_id}/projection?recent_limit=500"),
        None,
    )
    .await;
    assert!(projection["run_event_high_watermark"].as_u64().unwrap() < 500);
    let events = projection["recent_events"].as_array().unwrap();
    let applied = events
        .iter()
        .find(|event| {
            event["type"] == "user_intent_applied"
                && event["run_id"] == child_id
                && event["content"] == guidance
                && event["delivery"] == "guide_current_run"
                && event["status"] == "applied"
        })
        .expect("member consumed guidance");
    let intent_id = applied["intent_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .unwrap();
    let accepted = events
        .iter()
        .find(|event| {
            event["type"] == "user_intent_accepted"
                && event["intent_id"] == intent_id
                && event["run_id"] == child_id
                && event["status"] == "accepted_remote"
        })
        .unwrap();
    let pause = events
        .iter()
        .find(|event| event["type"] == "run_paused" && event["run_id"] == child_id)
        .unwrap();
    let resume = events
        .iter()
        .find(|event| event["type"] == "run_resumed" && event["run_id"] == child_id)
        .unwrap();
    assert_eq!(applied["event_index"], accepted["index"]);
    let index = |event: &serde_json::Value| event["index"].as_u64().unwrap();
    assert!(
        index(pause) < index(accepted)
            && index(accepted) < index(resume)
            && index(resume) < index(applied),
        "guidance must be accepted while paused, then applied after resume"
    );
    assert!(
        events
            .iter()
            .filter(|event| event["type"] == "tool_request" && index(event) > index(resume))
            .any(
                |request| events.iter().any(|end| end["type"] == "tool_call_end"
                    && end["call_id"] == request["request_id"]
                    && end["success"] == true
                    && end["transport"] == "edge_ledger"
                    && end["result"]
                        .as_str()
                        .is_some_and(|result| result.to_ascii_lowercase().contains(&value)))
            ),
        "resume must observe the file-only random value in a successful fresh Edge invocation"
    );
    let transcript = live_agent_json(
        &client,
        &api,
        reqwest::Method::GET,
        &format!("/sessions/{session_id}/transcript?run_id={root_id}&limit=200"),
        None,
    )
    .await;
    assert_eq!(transcript["has_more"], false);
    let final_answer = transcript["items"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|item| item["role"] == "assistant")
        .unwrap();
    assert_eq!(final_answer["run_id"], root_id);
    assert_eq!(
        final_answer["content"].as_str().unwrap().trim(),
        value.to_uppercase()
    );
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_g_reopens_a_child_transcript_after_completion() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration(
        astra_cli::cli::mock_llm::MockScenario::AgentThenComplete,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.paste_and_submit("delegate_one_child_and_keep_it_observable");
    astra.wait_for("Parent acknowledged", UI_TRANSITION_TIMEOUT);
    astra.wait_for("1 live", UI_TRANSITION_TIMEOUT);
    astra.wait_for_screen(
        "root output and reported token usage together",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("Parent acknowledged")
                && screen.lines().any(|line| {
                    line.contains("Working") && line.contains("reported input 120 / output 8 tok")
                })
        },
    );
    astra.write(&[0x07]);
    astra.wait_for_screen("run navigator", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("Agent runs") || screen.contains("Conversations")
    });
    astra.wait_for("1. Mock child review", UI_TRANSITION_TIMEOUT);
    astra.select_conversation_run("mock-run-agent-child", 1, None);
    astra.write(b"\r");
    astra.wait_for("child_evidence_visible", UI_TRANSITION_TIMEOUT);
    astra.wait_for_screen("selected child activity", UI_TRANSITION_TIMEOUT, |screen| {
        screen.contains("child_evidence_visible")
            && screen.lines().any(|line| {
                line.contains("Mock child review")
                    && ["working", "starting", "waiting"]
                        .iter()
                        .any(|state| line.contains(state))
            })
    });
    mock.release_held_response();
    astra.wait_for_screen(
        "child completion in the open conversation",
        UI_TRANSITION_TIMEOUT,
        |screen| {
            screen.contains("child_evidence_visible")
                && screen
                    .lines()
                    .any(|line| line.contains("done") && line.contains("Mock child review"))
        },
    );
    astra.write(&[0x0f]);
    astra.wait_for("Parent acknowledged", UI_TRANSITION_TIMEOUT);
    astra.wait_for_absent("1 live", UI_TRANSITION_TIMEOUT);
    for _ in 0..2 {
        astra.select_completed_conversation("Mock child review");
        astra.wait_for("child_evidence_visible", UI_TRANSITION_TIMEOUT);
        astra.write(&[0x0f]);
        astra.wait_for("Main conversation ·", UI_TRANSITION_TIMEOUT);
    }
    assert_eq!(
        mock.received_requests().len(),
        1,
        "Server owns child execution"
    );
    assert!(mock.tool_results().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fanout_partial_refresh_preserves_slot_selection() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration(
        astra_cli::cli::mock_llm::MockScenario::FanoutThenComplete,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.write(b"launch_three_reviews_as_one_group\r");
    astra.wait_for("Three mock reviews are running", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x07]);
    astra.wait_for("Agent runs", UI_TRANSITION_TIMEOUT);
    for slot in 1..=3 {
        astra.wait_for(&format!("Mock review {slot}"), UI_TRANSITION_TIMEOUT);
    }
    select_task_slot(&mut astra, "2: Mock review 2", UI_TRANSITION_TIMEOUT);
    let selected = selected_task_slot(&astra.current_screen()).unwrap();
    astra.wait_for("stop available (X)", UI_TRANSITION_TIMEOUT);
    mock.publish_partial_fanout_results();
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while !astra.current_screen().contains("2 done") {
        assert!(
            Instant::now() < deadline,
            "busy input starved durable run observation"
        );
        astra.write(b"2"); // Re-select the same slot without an explicit refresh.
        let next_key = Instant::now() + Duration::from_millis(25);
        while Instant::now() < next_key {
            astra.receive(next_key.saturating_duration_since(Instant::now()));
        }
    }
    assert_eq!(
        selected_task_slot(&astra.current_screen()).as_ref(),
        Some(&selected)
    );
    let screen = astra.current_screen();
    for slot in 1..=3 {
        assert_eq!(
            screen
                .matches(&format!("slot {slot}: Mock review {slot}"))
                .count(),
            1
        );
    }
    astra.write(b"\x1b");
    astra.wait_for_absent("Agent runs", UI_TRANSITION_TIMEOUT);
    mock.release_held_response();
    astra.wait_for("Parent reconciled one", UI_TRANSITION_TIMEOUT);
    assert_eq!(
        mock.received_requests().len(),
        1,
        "no client reconciliation admission"
    );
    assert!(mock.tool_results().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_fanout_slot_cause_is_visible_in_its_transcript() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration(
        astra_cli::cli::mock_llm::MockScenario::FanoutPartialThenComplete,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.write(b"review_with_one_failed_slot\r");
    astra.wait_for("Three mock reviews are running", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x07]);
    astra.wait_for("Agent runs", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Mock review 2", UI_TRANSITION_TIMEOUT);
    select_task_slot(&mut astra, "2: Mock review 2", UI_TRANSITION_TIMEOUT);
    mock.publish_partial_fanout_results();
    astra.write(b"r");
    astra.wait_for("1 failed", UI_TRANSITION_TIMEOUT);
    mock.release_held_response();
    astra.write(b"\r");
    astra.wait_for(
        "fanout_child_2_failed_with_distinct_cause",
        UI_TRANSITION_TIMEOUT,
    );
    astra.write(&[0x0f]);
    astra.wait_for("Parent reconciled 2 completed", UI_TRANSITION_TIMEOUT);
    assert_eq!(mock.received_requests().len(), 1);
    assert!(mock.tool_results().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_child_cancel_keeps_sibling_transcript_queryable() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start_with_held_orchestration(
        astra_cli::cli::mock_llm::MockScenario::FanoutThenComplete,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", Duration::from_secs(15));
    astra.write(b"inspect_and_cancel_one_child\r");
    astra.wait_for("Three mock reviews are running", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x07]);
    astra.wait_for("Agent runs", UI_TRANSITION_TIMEOUT);
    astra.wait_for("Mock review 1", UI_TRANSITION_TIMEOUT);
    select_task_slot(&mut astra, "1: Mock review 1", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r");
    astra.wait_for("fanout_child_1_evidence_visible", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x07]);
    astra.wait_for("Agent runs", UI_TRANSITION_TIMEOUT);
    select_task_slot(&mut astra, "3: Mock review 3", UI_TRANSITION_TIMEOUT);
    astra.write(b"r");
    astra.wait_for("stop available", UI_TRANSITION_TIMEOUT);
    astra.write(b"x");
    let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
    while mock.cancellation_requests().is_empty() {
        assert!(
            Instant::now() < deadline,
            "cancel not received: {}",
            astra.screen_diagnostic()
        );
        astra.receive(Duration::from_millis(25));
        tokio::task::yield_now().await;
    }
    assert_eq!(mock.cancellation_requests(), ["mock-run-fanout-child-2"]);
    astra.write(b"\x1b");
    astra.wait_for_absent("Agent runs", UI_TRANSITION_TIMEOUT);
    mock.release_held_response();
    astra.write(&[0x0f]);
    astra.wait_for("including cancellation", UI_TRANSITION_TIMEOUT);
    astra.write(&[0x07]);
    astra.wait_for("Agent runs", UI_TRANSITION_TIMEOUT);
    astra.write(b"h");
    astra.wait_for("Mock review 1", UI_TRANSITION_TIMEOUT);
    select_task_slot(&mut astra, "1: Mock review 1", UI_TRANSITION_TIMEOUT);
    astra.write(b"\r");
    astra.wait_for("fanout_child_1_evidence_visible", UI_TRANSITION_TIMEOUT);
    assert_eq!(mock.received_requests().len(), 1);
    assert!(mock.tool_results().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_plan_goal_keeps_prior_snapshot_as_reference() {
    let _journey = pty_journey_lock().lock().await;
    let mock = astra_cli::cli::mock_llm::MockLlmServer::start(
        astra_cli::cli::mock_llm::MockScenario::Slow,
    )
    .await
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_trusted_workspace(home.path());
    seed_account(home.path());
    let mut astra = PtyAstra::spawn(home.path(), &mock.base_url);
    astra.wait_for("Message Astra", UI_TRANSITION_TIMEOUT);

    for (index, command) in [
        "/plan",
        "first objective",
        "/plan second objective",
        "/plan",
        "/plan first objective",
    ]
    .into_iter()
    .enumerate()
    {
        if index == 2 {
            astra.paste_and_submit("/allow auto");
            astra.wait_for("Mode → Auto", UI_TRANSITION_TIMEOUT);
        }
        astra.paste_and_submit(command);
        let deadline = Instant::now() + UI_TRANSITION_TIMEOUT;
        while mock.received_requests().len() <= index {
            assert!(
                Instant::now() < deadline,
                "plan request {index} did not reach admission\n{}",
                astra.screen_diagnostic()
            );
            astra.receive(Duration::from_millis(25));
            tokio::task::yield_now().await;
        }
        if index == 2 {
            let screen = astra.screen_diagnostic();
            assert!(
                !screen.contains("Plan mode active - goal: first objective"),
                "re-entering Plan must not announce the previous goal: {screen}"
            );
        }
        let requests = mock.received_requests();
        let request = &requests[index];
        let message = request["message"].as_str().expect("admitted message");
        match index {
            0 | 3 => assert!(message.contains("current task"), "{request}"),
            1 | 4 => assert!(message.contains("first objective"), "{request}"),
            2 => assert!(message.contains("second objective"), "{request}"),
            _ => unreachable!(),
        }
        let context = request["runtime_system_prompt"]
            .as_str()
            .expect("runtime context");
        assert!(context.contains("client plan snapshot"), "{context}");
        assert!(context.contains("may predate"), "{context}");
        assert!(
            !context.contains("plan authoring: [plan-resume]"),
            "{context}"
        );
        if index == 0 {
            assert!(!context.contains("Pause implementation"), "{context}");
        } else {
            assert!(context.contains("first objective"), "{context}");
        }
        astra.wait_for(&format!("turn {}", index + 1), UI_TRANSITION_TIMEOUT);
    }
    astra.signal(nix::sys::signal::Signal::SIGHUP);
    assert!(astra.wait_for_exit(Duration::from_secs(10)).success());
}
