//! Installed-process boundary with a synthetic authenticated Server and local
//! provider. No real key, external service, or MatrixOne instance is required.
#![cfg(unix)]

use astra_credentials::{
    CredentialStore, LocalCredentialRef, LocalInferenceProtocol, LocalModelConfig,
    LocalModelDefinition, LocalModelProbeState, LocalModelScope, Profile,
};
use astra_edge::local_host::{Installation, ManagedClient};
use astra_server_types::edge_ws_protocol::{EdgeClientMessage, EdgeServerMessage};
use astra_turn_types::runner_inference::*;
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use std::num::NonZeroU64;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;

const OWNER: &str = "managed-process-fixture";
const REQUEST: &str = r#"{"model":"fixture","messages":[{"role":"user","content":"synthetic request"}],"max_tokens":16,"stream":true}"#;

fn id(value: &str) -> RunnerInferenceId {
    RunnerInferenceId::new(value).unwrap()
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The provider cannot finish until the test has observed a real progress
/// frame at the Server socket. A buffered terminal cannot satisfy this gate.
struct ControlledProvider {
    uri: String,
    calls: Arc<AtomicUsize>,
    finish: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl ControlledProvider {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let finish = Arc::new(Notify::new());
        let seen = calls.clone();
        let release = finish.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let release = release.clone();
                let seen = seen.clone();
                connections.spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    loop {
                        let n = socket.read(&mut buffer).await.unwrap();
                        assert!(n > 0, "provider request ended before its exact body");
                        request.extend_from_slice(&buffer[..n]);
                        assert!(request.len() < 16 * 1024);
                        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                            && request.len() >= end + 4 + REQUEST.len()
                        {
                            assert_eq!(&request[end + 4..end + 4 + REQUEST.len()], REQUEST.as_bytes());
                            break;
                        }
                    }
                    seen.fetch_add(1, Ordering::SeqCst);
                    let first = "data: {\"choices\":[{\"delta\":{\"content\":\"synthetic reply\"}}]}\n\n";
                    let last = "data: [DONE]\n\n";
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first}", first.len() + last.len()).as_bytes()).await.unwrap();
                    socket.flush().await.unwrap();
                    release.notified().await;
                    socket.write_all(last.as_bytes()).await.unwrap();
                });
            }
        });
        Self {
            uri,
            calls,
            finish,
            task,
        }
    }
}

impl Drop for ControlledProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Connection {
    journal: RunnerInferenceId,
    boot: RunnerInferenceId,
    generation: u64,
    send: mpsc::Sender<EdgeServerMessage>,
    receive: mpsc::Receiver<EdgeClientMessage>,
}

impl Connection {
    async fn next(&mut self) -> EdgeClientMessage {
        tokio::time::timeout(Duration::from_secs(15), self.receive.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn binding(&mut self) -> RunnerInferenceBindingIdentity {
        loop {
            if let EdgeClientMessage::InferenceBindingPublish { publication } = self.next().await
                && let RunnerInferenceBindingChange::Publish { definition } = publication.change
            {
                return definition.identity;
            }
        }
    }

    async fn terminal(&mut self) -> RunnerInferenceTerminalTransfer {
        loop {
            if let EdgeClientMessage::InferenceTerminal { transfer, .. } = self.next().await {
                return *transfer;
            }
        }
    }
}

// tungstenite's Callback contract fixes the unboxed HTTP error-response type.
#[allow(clippy::result_large_err)]
async fn accept(listener: &TcpListener, generation: u64, expected_token: &str) -> Connection {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let expected_authorization = format!("Bearer {expected_token}");
    let mut socket = tokio_tungstenite::accept_hdr_async(
        stream,
        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
         response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            assert!(
                request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    == Some(expected_authorization.as_str()),
                "managed host must authenticate with the current selected profile token"
            );
            Ok(response)
        },
    )
    .await
    .unwrap();
    let auth = socket.next().await.unwrap().unwrap();
    let auth: EdgeClientMessage = serde_json::from_str(auth.to_text().unwrap()).unwrap();
    assert!(
        matches!(
            auth,
            EdgeClientMessage::Auth {
                workspace_dir: None,
                capabilities: None,
                ..
            }
        ),
        "inference-only process must not advertise tools"
    );
    socket
        .send(Message::Text(
            serde_json::to_string(&EdgeServerMessage::AuthOk {
                user_id: OWNER.into(),
                interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let (journal, boot, version) = loop {
        let frame = socket.next().await.unwrap().unwrap();
        if let Ok(EdgeClientMessage::InferenceHello {
            journal_id,
            process_boot_nonce,
            protocol_version,
        }) = serde_json::from_str(frame.to_text().unwrap_or_default())
        {
            break (journal_id, process_boot_nonce, protocol_version);
        }
    };
    socket
        .send(Message::Text(
            serde_json::to_string(&EdgeServerMessage::InferenceHelloAck {
                negotiation: RunnerInferenceNegotiation::accepted(version, generation, now_ms()),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let (send, mut outgoing) = mpsc::channel::<EdgeServerMessage>(32);
    let (incoming, receive) = mpsc::channel(128);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                message = outgoing.recv() => {
                    let Some(message) = message else { break; };
                    if socket.send(Message::Text(serde_json::to_string(&message).unwrap().into())).await.is_err() { break; }
                }
                frame = socket.next() => {
                    let Some(Ok(Message::Text(frame))) = frame else { break; };
                    let Ok(message) = serde_json::from_str::<EdgeClientMessage>(&frame) else { break; };
                    if let EdgeClientMessage::InferenceBindingPublish { publication } = &message {
                        let receipt = RunnerInferenceBindingReceipt { operation_id: publication.operation_id.clone(), publication_revision: NonZeroU64::new(publication.expected_publication_revision + 1).unwrap(), identity: publication.change.identity().clone() };
                        if socket.send(Message::Text(serde_json::to_string(&EdgeServerMessage::InferenceBindingAck { receipt }).unwrap().into())).await.is_err() { break; }
                    }
                    if incoming.send(message).await.is_err() { break; }
                }
            }
        }
    });
    Connection {
        journal,
        boot,
        generation,
        send,
        receive,
    }
}

fn spawn(origin: &str, credentials: &std::path::Path) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_astra-edge"));
    command
        .env_clear()
        .env("ASTRA_CLI_CREDENTIALS_DIR", credentials)
        .args([
            "--inference-only",
            "--managed-inference-host",
            "--expected-inference-owner",
            OWNER,
            "--server-url",
            origin,
            "--profile",
            "fixture",
            "--reconnect=true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in astra_core::net::RUNNER_NETWORK_ENV_VARS {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.spawn().unwrap()
}

#[tokio::test]
async fn managed_process_reopens_same_journal_after_ack_loss_without_provider_redispatch() {
    let directory = tempfile::tempdir().unwrap();
    let _credentials = astra_credentials::set_test_credentials_dir(directory.path().to_owned());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    CredentialStore::new()
        .mutate(|file| {
            file.current_profile = Some("fixture".into());
            file.profiles.insert(
                "fixture".into(),
                Profile {
                    account_id: Some(OWNER.into()),
                    access_token: Some("synthetic-fixture-token".into()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    let provider = ControlledProvider::start().await;
    let scope = LocalModelScope::for_owner(&origin, OWNER).unwrap();
    let mut config = LocalModelConfig::default();
    config.models.insert(
        "Work".into(),
        LocalModelDefinition {
            protocol: LocalInferenceProtocol::OpenaiCompatible,
            base_url: provider.uri.clone(),
            model: "fixture".into(),
            binding_revision: 1,
            context_window: 1024,
            max_output_tokens: 16,
            credential: LocalCredentialRef::None,
            probe: LocalModelProbeState::default(),
        },
    );
    scope.models().replace(0, config).unwrap();
    let installation = Installation::open(&scope).unwrap();
    let mut first = spawn(&origin, directory.path());
    let mut connection = accept(&listener, 1, "synthetic-fixture-token").await;
    let a = ManagedClient::connect(&installation, scope.clone(), &origin, Some("fixture"))
        .await
        .unwrap();
    let b = ManagedClient::connect(&installation, scope.clone(), &origin, Some("fixture"))
        .await
        .unwrap();
    let b_liveness = b.liveness();
    assert!(b_liveness.is_alive());
    assert_ne!(a.attachment.lease_id, b.attachment.lease_id);
    drop(a);
    assert!(
        first.try_wait().unwrap().is_none(),
        "closing the first view must not stop shared host"
    );
    let binding = connection.binding().await;
    let live_journal = connection.journal.clone();
    let live_boot = connection.boot.clone();
    CredentialStore::new()
        .mutate(|file| {
            file.profiles.get_mut("fixture").unwrap().access_token =
                Some("synthetic-rotated-token".into());
        })
        .unwrap();
    // Dropping the synthetic Server socket forces a reconnect in the SAME
    // child, proving that it reloads credentials rather than using startup's
    // token. Client leases and journal identity remain unchanged.
    drop(connection);
    let mut connection = accept(&listener, 2, "synthetic-rotated-token").await;
    assert_eq!(connection.journal, live_journal);
    assert_eq!(connection.boot, live_boot);
    assert!(first.try_wait().unwrap().is_none());
    assert!(b_liveness.is_alive());
    let grant = RunnerInferenceDispatchGrant {
        attempt: RunnerInferenceAttemptIdentity {
            user_id: OWNER.into(),
            scope: astra_turn_types::InferenceInvocationScope::Session {
                session_id: "fixture-session".into(),
                turn: 0,
                round: 0,
                operation_id: "fixture-operation".into(),
                logical_attempt: 0,
            },
            invocation_id: id("fixture-invocation"),
            attempt_id: id("fixture-attempt"),
            binding,
            request: RunnerInferenceArtifactReference {
                artifact_id: id("fixture-request"),
                sha256: RunnerInferenceDigest::new(format!(
                    "{:x}",
                    Sha256::digest(REQUEST.as_bytes())
                ))
                .unwrap(),
                byte_len: NonZeroU64::new(REQUEST.len() as u64).unwrap(),
            },
        },
        grant_id: id("fixture-grant"),
        process_boot_nonce: connection.boot.clone(),
        start_before_unix_ms: now_ms() + 15_000,
        deadline_unix_ms: now_ms() + 30_000,
    };
    connection
        .send
        .send(EdgeServerMessage::InferenceDispatch {
            grant: Box::new(grant.clone()),
            delivery_generation: connection.generation,
        })
        .await
        .unwrap();
    loop {
        if matches!(
            connection.next().await,
            EdgeClientMessage::InferenceRequestCredit { .. }
        ) {
            break;
        }
    }
    connection
        .send
        .send(EdgeServerMessage::InferenceRequestChunk {
            attempt_id: grant.attempt.attempt_id.clone(),
            delivery_generation: connection.generation,
            chunk: RunnerInferencePayloadChunk {
                offset: 0,
                data: RunnerInferenceChunkData::new(REQUEST.into()).unwrap(),
            },
        })
        .await
        .unwrap();
    loop {
        match connection.next().await {
            EdgeClientMessage::InferenceProgress {
                progress,
                delivery_generation,
            } => {
                assert_eq!(delivery_generation, connection.generation);
                assert_eq!(progress.attempt, grant.attempt);
                assert!(progress.events.iter().any(|event| matches!(
                    &event.event,
                    RunnerInferenceProviderEvent::Json(value)
                        if value.pointer("/choices/0/delta/content").and_then(serde_json::Value::as_str) == Some("synthetic reply")
                )));
                break;
            }
            EdgeClientMessage::InferenceTerminal { .. } => {
                panic!("terminal arrived before provider was released")
            }
            _ => {}
        }
    }
    provider.finish.notify_one();
    let terminal = connection.terminal().await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    first.kill().await.unwrap(); // simulate crash before any terminal ACK
    tokio::time::timeout(Duration::from_secs(5), async {
        while b_liveness.is_alive() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("existing window must observe the failed connection");
    drop(b);
    let old_journal = connection.journal;
    let old_boot = connection.boot;
    let mut second = spawn(&origin, directory.path());
    let mut recovered = accept(&listener, 3, "synthetic-rotated-token").await;
    assert_eq!(recovered.journal, old_journal);
    assert_ne!(recovered.boot, old_boot);
    let _view = ManagedClient::connect(&installation, scope, &origin, Some("fixture"))
        .await
        .unwrap();
    assert!(_view.liveness().is_alive());
    assert!(
        !b_liveness.is_alive(),
        "reconnect must not revive the old lease"
    );
    let replay = recovered.terminal().await;
    assert_eq!(replay.attempt, terminal.attempt);
    assert_eq!(replay.terminal_sha256, terminal.terminal_sha256);
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "custody replay must not reopen provider transport"
    );
    second.kill().await.unwrap();
    // The subprocess was killed deliberately, so it cannot unlink its socket.
    // Remove only this fixture's socket and now-empty runtime directory.
    std::fs::remove_file(installation.socket_path()).unwrap();
    std::fs::remove_dir(installation.socket_path().parent().unwrap()).unwrap();
}
