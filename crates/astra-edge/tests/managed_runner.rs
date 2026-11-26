#![cfg(unix)]

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use astra_server_types::AGENT_INTERACTION_API_MAJOR;
use astra_server_types::edge_ws_protocol::{
    EdgeClientMessage, EdgeServerMessage, ToolInvocationIdentity,
};
use futures_util::{SinkExt, StreamExt};
use nix::pty::openpty;
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

const EDGE_ID: &str = "runner-pty-test";

struct Runner {
    root: tempfile::TempDir,
    server: String,
}

impl Runner {
    fn new(listener: &TcpListener) -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            server: format!("ws://{}/edge/ws", listener.local_addr().unwrap()),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_astra-edge"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.path())
            .env("ASTRA_LOCAL_STATE_ROOT", self.root.path().join("state"))
            .current_dir(self.root.path())
            .env("ASTRA_WORKSPACE_DIR", self.root.path());
        command
    }

    fn manage(&self, action: &str) -> Output {
        self.command()
            .args([action, "--edge-id", EDGE_ID, "--server-url", &self.server])
            .output()
            .unwrap()
    }

    fn launch(&self) -> (Child, File) {
        let pty = openpty(None, None).unwrap();
        let slave = File::from(pty.slave);
        let mut command = self.command();
        command
            .args([
                "--edge-id",
                EDGE_ID,
                "--server-url",
                &self.server,
                "--token",
                "synthetic-pty-credential",
                "--reconnect",
                "true",
            ])
            .stdin(slave)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the child only performs async-signal-safe system calls
        // before exec. Its stdin is the already-open PTY slave.
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() < 0 || nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        (command.spawn().unwrap(), File::from(pty.master))
    }

    fn phase(&self, expected: &str) {
        let output = self.manage("status");
        let status = String::from_utf8(output.stderr).unwrap();
        assert!(
            output.status.success() && status.contains(&format!("phase: {expected}")),
            "{status}"
        );
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.manage("stop");
    }
}

type Socket = WebSocketStream<TcpStream>;

async fn accept(listener: &TcpListener) -> Socket {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    assert!(
        matches!(receive(&mut socket).await, EdgeClientMessage::Auth { edge_agent_id, .. } if edge_agent_id == EDGE_ID)
    );
    socket
}

async fn authenticate(socket: &mut Socket) {
    send(
        socket,
        EdgeServerMessage::AuthOk {
            user_id: "user".into(),
            edge_id: EDGE_ID.into(),
            interaction_api_major: AGENT_INTERACTION_API_MAJOR.into(),
        },
    )
    .await;
}

async fn send(socket: &mut Socket, message: EdgeServerMessage) {
    socket
        .send(Message::Text(
            serde_json::to_string(&message).unwrap().into(),
        ))
        .await
        .unwrap();
}

async fn receive(socket: &mut Socket) -> EdgeClientMessage {
    loop {
        if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn wait_for_file(path: &Path) {
    while !path.exists() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_parent(parent: &mut Child) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = parent.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > Duration::from_secs(15) {
            let _ = parent.kill();
            let _ = parent.wait();
            panic!("connect command did not return");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn terminal_close_reconnect_and_stop_preserve_invocation_custody() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let runner = Runner::new(&listener);
        // A stale pre-server file must neither gate startup nor be imported.
        let mut legacy_key = Sha256::new();
        legacy_key.update(EDGE_ID.as_bytes());
        legacy_key.update([0]);
        legacy_key.update(
            runner
                .root
                .path()
                .canonicalize()
                .unwrap()
                .as_os_str()
                .as_encoded_bytes(),
        );
        let journals = runner.root.path().join("state/edge-invocations");
        std::fs::create_dir_all(&journals).unwrap();
        let stale = journals.join(format!("{:x}.json", legacy_key.finalize()));
        std::fs::write(&stale, b"unreadable old journal format").unwrap();

        let (mut parent, terminal) = runner.launch();
        let mut first = accept(&listener).await;
        assert!(
            parent.try_wait().unwrap().is_none(),
            "connect returned before authentication"
        );
        authenticate(&mut first).await;
        assert!(wait_for_parent(&mut parent).await.success());
        drop(terminal);
        runner.phase("connected");
        let logs = runner.manage("logs");
        assert!(logs.status.success());
        let logs = String::from_utf8(logs.stdout).unwrap();
        assert!(logs.contains("authenticating"));
        assert!(!logs.contains("synthetic-pty-credential"));

        let identity =
            ToolInvocationIdentity::new("user", "session", "run", "chain", "pty-call").unwrap();
        let request_id = identity.storage_key();
        send(
            &mut first,
            EdgeServerMessage::ToolRequest {
                approved_bash_write: None,
                request_id: request_id.clone(),
                identity: Box::new(identity),
                delivery_generation: 1,
                tool: "bash".into(),
                args: serde_json::json!({"command": "touch started; sleep 30; touch leaked"}),
                runtime_process_authorization: None,
                runtime_process_authorization_required: false,
                timeout_secs: Some(30),
                execution_deadline_unix_ms: None,
                execution_timeout_ms: None,
                command_timeout_cap_ms: None,
                execution_ceiling: None,
                traceparent: None,
                tracestate: None,
            },
        )
        .await;
        wait_for_file(&runner.root.path().join("started")).await;
        drop(first);

        let mut second = accept(&listener).await;
        authenticate(&mut second).await;
        loop {
            if let EdgeClientMessage::ToolResult {
                request_id: received,
                delivery_generation,
                is_error,
                output,
                ..
            } = receive(&mut second).await
            {
                assert_eq!(received, request_id);
                assert_eq!(delivery_generation, 1);
                assert!(is_error && output.contains("cancelled"));
                break;
            }
        }
        send(
            &mut second,
            EdgeServerMessage::ToolResultAck {
                request_id,
                delivery_generation: 1,
            },
        )
        .await;
        send(
            &mut second,
            EdgeServerMessage::Closing {
                reason: "test reconnect after result acknowledgement".into(),
            },
        )
        .await;
        let mut third = accept(&listener).await;
        authenticate(&mut third).await;
        assert!(runner.manage("stop").status.success());
        runner.phase("stopped");
        assert!(!runner.root.path().join("leaked").exists());
        let mut current_key = Sha256::new();
        current_key.update(EDGE_ID.as_bytes());
        current_key.update([0]);
        current_key.update(runner.server.as_bytes());
        current_key.update([0]);
        current_key.update(
            runner
                .root
                .path()
                .canonicalize()
                .unwrap()
                .as_os_str()
                .as_encoded_bytes(),
        );
        let current = journals.join(format!("{:x}.json", current_key.finalize()));
        assert!(current.with_extension("json.wal").exists());
        assert!(!astra_edge::has_pending_invocation_results(current).await);
        assert_eq!(
            std::fs::read(&stale).unwrap(),
            b"unreadable old journal format"
        );
        assert!(!stale.with_extension("json.lock").exists());
    })
    .await
    .expect("managed runner journey timed out");
}

#[tokio::test]
async fn rejected_startup_returns_failure_and_retires_the_child() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let runner = Runner::new(&listener);
        let (mut parent, _terminal) = runner.launch();
        let mut socket = accept(&listener).await;
        send(
            &mut socket,
            EdgeServerMessage::AuthError {
                message: "test rejection".into(),
            },
        )
        .await;
        assert!(!wait_for_parent(&mut parent).await.success());
        assert!(
            socket
                .next()
                .await
                .is_none_or(|frame| !matches!(frame, Ok(Message::Text(_))))
        );
        runner.phase("authentication-failure");
        assert!(runner.manage("stop").status.success());
    })
    .await
    .expect("authentication failure journey timed out");
}
