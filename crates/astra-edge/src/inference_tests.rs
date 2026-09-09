use super::*;
use crate::inference_connection::{InferenceConnection, MAX_PENDING_PROGRESS_FOR_TEST};
use astra_credentials::{LocalInferenceProtocol, LocalModelDefinition, LocalModelProbeState};
use astra_server_types::edge_ws_protocol::{EdgeClientMessage, EdgeServerMessage};
use std::sync::atomic::{AtomicBool, Ordering};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[path = "managed_host_tests.rs"]
mod managed_host_tests;

struct Fixture {
    directory: tempfile::TempDir,
    host: Arc<InferenceHost>,
    clock: GrantClock,
}

#[tokio::test]
async fn concurrent_preparation_does_not_block_cancel_and_aborted_capacity_is_released() {
    let fixture = Fixture::new("http://127.0.0.1:1").await;
    let first = fixture.grant("preparing-a", REQUEST).await;
    let second = fixture.grant("preparing-b", REQUEST).await;
    // Hold attachment mutation open to stop preparation before the fence.
    let attachment = fixture.host.state.write().await;
    let spawn = |grant: RunnerInferenceDispatchGrant| {
        let host = fixture.host.clone();
        let clock = fixture.clock;
        tokio::spawn(async move { host.dispatch(grant, REQUEST.into(), clock).await })
    };
    let first_task = spawn(first.clone());
    let second_task = spawn(second);
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.host.active_count().await != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let host = fixture.host.clone();
    let cancelled = first.clone();
    let cancel_task = tokio::spawn(async move { host.cancel(&cancelled).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fixture
                .host
                .active
                .lock()
                .unwrap()
                .get(first.attempt.attempt_id.as_str())
                .unwrap()
                .cancellation
                .is_cancelled()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    second_task.abort();
    assert!(second_task.await.unwrap_err().is_cancelled());
    assert_eq!(fixture.host.active_count().await, 1);
    drop(attachment);
    assert!(matches!(
        first_task.await.unwrap().unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::CancelledWithoutFence)
    ));
    assert!(matches!(
        cancel_task.await.unwrap().unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::CancelledWithoutFence)
    ));
    assert_eq!(fixture.host.active_count().await, 0);
    assert!(matches!(
        fixture
            .host
            .dispatch(first, REQUEST.into(), fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::CancelledWithoutFence)
    ));
}

#[tokio::test]
async fn forged_cancel_cannot_signal_preparing_attempt() {
    let fixture = Fixture::new("http://127.0.0.1:1").await;
    let grant = fixture.grant("preparing-owner", REQUEST).await;
    let control = fixture.host.attempt_lock(&grant).unwrap();
    let mut forged = grant.clone();
    forged.grant_id = id("another-grant");
    assert!(matches!(
        fixture.host.cancel(&forged).await,
        Err(InferenceHostError::IdentityConflict)
    ));
    assert!(!control.cancellation.is_cancelled());
}

fn id(value: &str) -> RunnerInferenceId {
    RunnerInferenceId::new(value).unwrap()
}

async fn worker_dispatch(
    fixture: &Fixture,
    grant: &RunnerInferenceDispatchGrant,
) -> crate::inference_connection::InferenceConnectionWorker {
    let mut worker =
        crate::inference_connection::InferenceConnectionWorker::spawn(fixture.host.clone());
    assert!(matches!(
        worker.messages.recv().await,
        Some(EdgeClientMessage::InferenceHello { .. })
    ));
    worker
        .commands
        .send(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::accepted(
                RUNNER_INFERENCE_PROTOCOL_VERSION,
                7,
                1_000_000,
            ),
        })
        .await
        .unwrap();
    worker
        .commands
        .send(EdgeServerMessage::InferenceDispatch {
            grant: Box::new(grant.clone()),
            delivery_generation: 7,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(
            worker.messages.recv().await,
            Some(EdgeClientMessage::InferenceRequestCredit { .. })
        ) {}
    })
    .await
    .unwrap();
    worker
        .commands
        .send(EdgeServerMessage::InferenceRequestChunk {
            attempt_id: grant.attempt.attempt_id.clone(),
            delivery_generation: 7,
            chunk: RunnerInferencePayloadChunk {
                offset: 0,
                data: RunnerInferenceChunkData::new(REQUEST.into()).unwrap(),
            },
        })
        .await
        .unwrap();
    worker
}

#[tokio::test]
async fn worker_cancel_and_preview_progress_while_dispatch_preparation_is_paused() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let grant = fixture.grant("slow-preparation", REQUEST).await;
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    *fixture.host.preparation_pause.lock().unwrap() = Some((entered, resume));
    let mut worker = worker_dispatch(&fixture, &grant).await;
    tokio::time::timeout(Duration::from_secs(2), entry)
        .await
        .unwrap()
        .unwrap();
    fixture
        .host
        .preview_tx
        .send(InferencePreview {
            attempt: grant.attempt.clone(),
            sequence: 0,
            event: RunnerInferenceProviderEvent::Json(
                serde_json::json!({"choices":[{"delta":{"content":"preview"}}]}),
            ),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(
            worker.messages.recv().await,
            Some(EdgeClientMessage::InferenceProgress { .. })
        ) {}
    })
    .await
    .unwrap();
    worker
        .commands
        .send(EdgeServerMessage::InferenceCancel {
            grant: Box::new(grant.clone()),
            delivery_generation: 7,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture
            .host
            .active
            .lock()
            .unwrap()
            .get(grant.attempt.attempt_id.as_str())
            .unwrap()
            .cancellation
            .is_cancelled()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release.send(()).unwrap();
    let _terminal = fixture.terminal().await;
    assert!(matches!(
        fixture.host.reconcile(&grant).await.unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::CancelledWithoutFence)
    ));
}

#[tokio::test]
async fn worker_ingress_signals_cancel_even_when_maintenance_waits_for_configuration() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let grant = fixture.grant("maintenance-cancel", REQUEST).await;
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    *fixture.host.preparation_pause.lock().unwrap() = Some((entered, resume));
    let worker = worker_dispatch(&fixture, &grant).await;
    tokio::time::timeout(Duration::from_secs(2), entry)
        .await
        .unwrap()
        .unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.directory.path().join("models.json.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    release.send(()).unwrap();
    // The periodic publication/configuration poll now also waits for this
    // lease. Cancellation ingress must remain independent of that actor.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    worker
        .commands
        .send(EdgeServerMessage::InferenceCancel {
            grant: Box::new(grant.clone()),
            delivery_generation: 7,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !fixture
            .host
            .active
            .lock()
            .unwrap()
            .get(grant.attempt.attempt_id.as_str())
            .unwrap()
            .cancellation
            .is_cancelled()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(lock);
    let _terminal = fixture.terminal().await;
    assert!(matches!(
        fixture.host.reconcile(&grant).await.unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::CancelledWithoutFence)
    ));
}

#[tokio::test]
async fn dropping_worker_during_blocking_fence_still_produces_terminal_custody() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let grant = fixture.grant("fence-disconnect", REQUEST).await;
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    *fixture.host.fence_pause.lock().unwrap() = Some((entered, resume));
    let worker = worker_dispatch(&fixture, &grant).await;
    tokio::time::timeout(Duration::from_secs(2), entry)
        .await
        .unwrap()
        .unwrap();
    drop(worker);
    tokio::task::yield_now().await;
    release.send(()).unwrap();
    let _terminal = fixture.terminal().await;
    assert!(matches!(
        fixture.host.reconcile(&grant).await.unwrap(),
        DispatchOutcome::Terminal(_)
    ));
    assert_eq!(fixture.host.active_count().await, 0);
}
fn owner() -> InferenceOwner {
    InferenceOwner {
        deployment_identity: "fixture-deployment".into(),
        user_id: "fixture-user".into(),
        runner_id: id("fixture-runner"),
    }
}
fn transport() -> ProviderTransport {
    ProviderTransport::build(reqwest::Client::builder().no_proxy()).unwrap()
}
impl Fixture {
    async fn new(endpoint: &str) -> Self {
        Self::new_with_credential(endpoint, LocalCredentialRef::None).await
    }

    async fn new_with_credential(endpoint: &str, credential: LocalCredentialRef) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let models_path = directory.path().join("models.json");
        let mut config = LocalModelConfig::default();
        config.models.insert(
            "local".into(),
            LocalModelDefinition {
                protocol: LocalInferenceProtocol::OpenaiCompatible,
                base_url: endpoint.into(),
                model: "fixture".into(),
                binding_revision: 1,
                context_window: 1024,
                max_output_tokens: 64,
                credential,
                probe: LocalModelProbeState::default(),
            },
        );
        LocalModelConfigStore::with_path(models_path.clone())
            .replace(0, config)
            .unwrap();
        let host = InferenceHost::open(
            directory.path().join("journal"),
            owner(),
            models_path,
            directory.path().join("secrets"),
            transport(),
        )
        .await
        .unwrap();
        let now = Instant::now();
        Self {
            directory,
            host,
            clock: GrantClock::observed(1_000_000, now, now).unwrap(),
        }
    }

    async fn grant(&self, attempt: &str, body: &str) -> RunnerInferenceDispatchGrant {
        RunnerInferenceDispatchGrant {
            attempt: RunnerInferenceAttemptIdentity {
                user_id: "fixture-user".into(),
                scope: astra_turn_types::InferenceInvocationScope::Session {
                    session_id: "fixture-session".into(),
                    turn: 0,
                    round: 0,
                    operation_id: "fixture-operation".into(),
                    logical_attempt: 0,
                },
                invocation_id: id("fixture-invocation"),
                attempt_id: id(attempt),
                binding: self.host.bindings().await.unwrap().remove(0).identity,
                request: RunnerInferenceArtifactReference {
                    artifact_id: id(attempt),
                    sha256: RunnerInferenceDigest::new(format!(
                        "{:x}",
                        Sha256::digest(body.as_bytes())
                    ))
                    .unwrap(),
                    byte_len: NonZeroU64::new(body.len() as u64).unwrap(),
                },
            },
            grant_id: id(attempt),
            process_boot_nonce: self.host.process_boot_nonce().clone(),
            start_before_unix_ms: 1_060_000,
            deadline_unix_ms: 1_120_000,
        }
    }

    async fn terminal(&self) -> RetainedTerminal {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some((_, payload)) = self.host.pending(1).await.unwrap().pop() {
                    return payload;
                }
                // Another connection may consume the best-effort wake-up
                // before this observer starts waiting. Keep a bounded polling
                // fallback so terminal custody is never hidden by a lost
                // notification in tests or in a reconnecting client.
                tokio::select! {
                    _ = self.host.terminal_ready() => {}
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                }
            }
        })
        .await
        .expect("causal provider completion")
    }
}

const REQUEST: &str = r#"{"model":"fixture","messages":[{"role":"user","content":"prompt-canary"}],"max_tokens":64,"stream":true}"#;

#[tokio::test]
async fn missing_local_credential_is_typed_no_start_and_never_provider_io() {
    let server = MockServer::start().await;
    let fixture = Fixture::new_with_credential(
        &server.uri(),
        LocalCredentialRef::ProtectedFile {
            secret_id: "absent_fixture_key".to_string(),
        },
    )
    .await;
    let grant = fixture.grant("missing-credential", REQUEST).await;

    assert!(matches!(
        fixture
            .host
            .dispatch(grant, REQUEST.into(), fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::RejectedWithoutFence)
    ));
    let payload = fixture.terminal().await;
    let response: RunnerInferenceResponse = serde_json::from_str(&payload.response_json).unwrap();
    assert_eq!(
        response.transport.status,
        RunnerInferenceTransportStatus::CredentialUnavailable
    );
    assert_eq!(
        payload.terminal.error_kind.as_deref(),
        Some("runner_credential_unavailable")
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn durable_fence_precedes_exact_single_http_call_and_ack_erases_only_payload() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(&server.uri()).await;
    let fence_path = fixture.directory.path().join("journal/attempt-exact.json");
    let saw_fence = Arc::new(AtomicBool::new(false));
    let observed = saw_fence.clone();
    Mock::given(method("POST")).respond_with(move |request: &wiremock::Request| {
        assert_eq!(request.body, REQUEST.as_bytes());
        let record: JournalRecord = serde_json::from_slice(&std::fs::read(&fence_path).unwrap()).unwrap();
        assert!(matches!(record.state, RecordState::ExecutionFenced));
        observed.store(true, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_string("data: {\"id\":\"response-id\",\"choices\":[{\"delta\":{\"content\":\"response-canary 🦀\"}}]}\n\ndata: [DONE]\n\n")
    }).expect(1).mount(&server).await;
    let grant = fixture.grant("exact", REQUEST).await;
    let (first, duplicate) = tokio::join!(
        fixture
            .host
            .dispatch(grant.clone(), REQUEST.into(), fixture.clock),
        fixture
            .host
            .dispatch(grant.clone(), REQUEST.into(), fixture.clock),
    );
    assert!(matches!(first.unwrap(), DispatchOutcome::Started));
    assert!(!matches!(duplicate.unwrap(), DispatchOutcome::Started));
    let payload = fixture.terminal().await;
    assert!(saw_fence.load(Ordering::SeqCst));
    let response: RunnerInferenceResponse = serde_json::from_str(&payload.response_json).unwrap();
    assert_eq!(
        response.transport.status,
        RunnerInferenceTransportStatus::Complete
    );
    assert!(matches!(
        response.events.last(),
        Some(RunnerInferenceProviderEvent::Eof)
    ));
    assert!(
        response
            .events
            .iter()
            .any(|event| matches!(event, RunnerInferenceProviderEvent::Done))
    );
    assert_eq!(payload.terminal.status, InferenceTerminalStatus::Succeeded);
    let diagnostics = format!("{:?} {:?}", payload, fixture.host);
    assert!(!diagnostics.contains("response-canary"));
    assert!(!diagnostics.contains("prompt-canary"));
    let bad_ack = RunnerInferenceTerminalAck {
        attempt: grant.attempt.clone(),
        terminal_sha256: RunnerInferenceDigest::new("0".repeat(64)).unwrap(),
    };
    assert_eq!(
        fixture.host.acknowledge(bad_ack).await.unwrap_err(),
        InferenceHostError::IdentityConflict
    );
    assert_eq!(
        fixture.terminal().await.terminal_sha256,
        payload.terminal_sha256
    );
    let ack = RunnerInferenceTerminalAck {
        attempt: grant.attempt.clone(),
        terminal_sha256: payload.terminal_sha256,
    };
    fixture.host.acknowledge(ack.clone()).await.unwrap();
    fixture.host.acknowledge(ack).await.unwrap();
    assert!(fixture.host.pending(1).await.unwrap().is_empty());
    let retained =
        std::fs::read_to_string(fixture.directory.path().join("journal/attempt-exact.json"))
            .unwrap();
    assert!(!retained.contains("response-canary"));
    assert!(!retained.contains("prompt-canary"));
    assert!(matches!(
        fixture
            .host
            .dispatch(grant.clone(), REQUEST.into(), fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::Acknowledged
    ));
}

#[tokio::test]
async fn runner_preview_is_attempt_keyed_and_does_not_replace_terminal_custody() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "data: {\"id\":\"preview-response\",\"choices\":[{\"delta\":{\"content\":\"live\"}}]}\n\ndata: [DONE]\n\n",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = Fixture::new(&server.uri()).await;
    let grant = fixture.grant("preview", REQUEST).await;
    let mut previews = fixture.host.subscribe_preview();

    assert!(matches!(
        fixture
            .host
            .dispatch(grant.clone(), REQUEST.into(), fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::Started
    ));

    let mut observed = Vec::new();
    for _ in 0..3 {
        observed.push(
            tokio::time::timeout(Duration::from_secs(2), previews.recv())
                .await
                .expect("preview should arrive")
                .expect("preview channel should remain open"),
        );
    }
    assert_eq!(
        observed
            .iter()
            .map(|preview| preview.sequence)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(observed.iter().all(|preview| {
        preview.attempt == grant.attempt && !format!("{preview:?}").contains("live")
    }));
    assert!(matches!(
        observed[0].event,
        RunnerInferenceProviderEvent::Json(_)
    ));
    assert!(matches!(
        observed[1].event,
        RunnerInferenceProviderEvent::Done
    ));
    assert!(matches!(
        observed[2].event,
        RunnerInferenceProviderEvent::Eof
    ));

    let payload = fixture.terminal().await;
    let response: RunnerInferenceResponse = serde_json::from_str(&payload.response_json).unwrap();
    assert_eq!(response.events.len(), 3);
    assert_eq!(
        response.transport.status,
        RunnerInferenceTransportStatus::Complete
    );
}

#[tokio::test]
async fn cancellation_before_fence_is_durable_no_start_not_absence() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(&server.uri()).await;
    let grant = fixture.grant("cancel", REQUEST).await;
    assert!(matches!(
        fixture.host.reconcile(&grant).await.unwrap(),
        DispatchOutcome::Unknown
    ));
    assert!(matches!(
        fixture.host.cancel(&grant).await.unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::CancelledWithoutFence)
    ));
    assert!(matches!(
        fixture
            .host
            .dispatch(grant.clone(), REQUEST.into(), fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::CancelledWithoutFence)
    ));
    let pending = fixture.host.pending(1).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, grant);
    let response: RunnerInferenceResponse =
        serde_json::from_str(&pending[0].1.response_json).unwrap();
    assert_eq!(
        response.transport.delivery,
        RunnerInferenceDeliveryEvidence::NotDispatched
    );
    assert_eq!(
        response.transport.status,
        RunnerInferenceTransportStatus::Cancelled
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn proven_connect_failure_is_a_definitive_physical_failure() {
    let response = RunnerInferenceResponse {
        events: Vec::new(),
        transport: RunnerInferenceTransportTerminal {
            status: RunnerInferenceTransportStatus::Transport,
            delivery: RunnerInferenceDeliveryEvidence::NotDispatched,
            provider_bytes: 0,
            events_delivered: 0,
        },
    };

    assert_eq!(
        physical_terminal(&response).status,
        InferenceTerminalStatus::Failed
    );
}

#[tokio::test]
async fn invalid_exact_material_is_rejected_but_forged_owner_or_boot_gets_no_proof() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(&server.uri()).await;
    for (name, change) in [("hash", 0), ("owner", 1), ("boot", 2), ("revision", 3)] {
        let mut grant = fixture.grant(name, REQUEST).await;
        match change {
            0 => grant.attempt.request.sha256 = RunnerInferenceDigest::new("0".repeat(64)).unwrap(),
            1 => grant.attempt.user_id = "forged-user".into(),
            2 => grant.process_boot_nonce = id("old-boot"),
            _ => grant.attempt.binding.profile_revision = NonZeroU64::new(2).unwrap(),
        }
        let result = fixture
            .host
            .dispatch(grant.clone(), REQUEST.into(), fixture.clock)
            .await;
        if change == 1 || change == 2 {
            assert!(result.is_err());
            assert!(
                !fixture
                    .directory
                    .path()
                    .join(format!("journal/attempt-{name}.json"))
                    .exists()
            );
        } else {
            assert!(matches!(
                result.unwrap(),
                DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::RejectedWithoutFence)
            ));
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_fence_poisoning_never_falls_back_to_provider_io() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(&server.uri()).await;
    fixture
        .host
        .with_journal(|journal| {
            journal.fail_next_commit();
            Ok(())
        })
        .await
        .unwrap();
    let grant = fixture.grant("io-fail", REQUEST).await;
    assert_eq!(
        fixture
            .host
            .dispatch(grant.clone(), REQUEST.into(), fixture.clock)
            .await
            .unwrap_err(),
        InferenceHostError::JournalIo
    );
    assert_eq!(
        fixture
            .host
            .dispatch(grant, REQUEST.into(), fixture.clock)
            .await
            .unwrap_err(),
        InferenceHostError::JournalIo
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn restart_converts_orphan_fence_to_unknown_without_resend() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(&server.uri()).await;
    let grant = fixture.grant("orphan", REQUEST).await;
    let stored = grant.clone();
    fixture
        .host
        .with_journal(move |journal| {
            assert!(journal.fence(&stored)?);
            Ok(())
        })
        .await
        .unwrap();
    let journal_id = fixture.host.journal_id().clone();
    let old_boot = fixture.host.process_boot_nonce().clone();
    let Fixture {
        directory, host, ..
    } = fixture;
    drop(host);
    let recovered = InferenceHost::open(
        directory.path().join("journal"),
        owner(),
        directory.path().join("models.json"),
        directory.path().join("secrets"),
        transport(),
    )
    .await
    .unwrap();
    assert_eq!(recovered.journal_id(), &journal_id);
    assert_ne!(recovered.process_boot_nonce(), &old_boot);
    let DispatchOutcome::Terminal(payload) = recovered.reconcile(&grant).await.unwrap() else {
        panic!("missing recovered terminal")
    };
    assert_eq!(
        payload.terminal.status,
        InferenceTerminalStatus::DeliveryUnknown
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn installation_lock_and_owner_scope_fail_closed() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let root = fixture.directory.path().join("journal");
    let models = fixture.directory.path().join("models.json");
    let secrets = fixture.directory.path().join("secrets");
    assert_eq!(
        InferenceHost::open(
            root.clone(),
            owner(),
            models.clone(),
            secrets.clone(),
            transport()
        )
        .await
        .unwrap_err(),
        InferenceHostError::AlreadyRunning
    );
    drop(fixture.host);
    let mut other = owner();
    other.user_id = "different-user".into();
    assert_eq!(
        InferenceHost::open(root, other, models, secrets, transport())
            .await
            .unwrap_err(),
        InferenceHostError::OwnerMismatch
    );
}

#[tokio::test]
async fn publication_operation_is_persisted_before_send_and_replayed_after_restart() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let pending = fixture.host.next_publication().await.unwrap().unwrap();
    assert_eq!(
        pending,
        fixture.host.next_publication().await.unwrap().unwrap()
    );
    let Fixture {
        directory, host, ..
    } = fixture;
    drop(host);
    let host = InferenceHost::open(
        directory.path().join("journal"),
        owner(),
        directory.path().join("models.json"),
        directory.path().join("secrets"),
        transport(),
    )
    .await
    .unwrap();
    assert_eq!(pending, host.next_publication().await.unwrap().unwrap());
    let RunnerInferenceBindingChange::Publish { definition } = &pending.change else {
        panic!("expected publish")
    };
    host.publication_ack(RunnerInferenceBindingReceipt {
        operation_id: pending.operation_id.clone(),
        publication_revision: NonZeroU64::new(1).unwrap(),
        identity: definition.identity.clone(),
    })
    .await
    .unwrap();
    assert!(host.next_publication().await.unwrap().is_none());
}

#[tokio::test]
async fn published_binding_keeps_friendly_and_provider_names_separate() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let bindings = fixture.host.bindings().await.unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].display_name.as_str(), "local");
    assert_eq!(bindings[0].model_name.as_str(), "fixture");
}

#[tokio::test(start_paused = true)]
async fn transient_enrollment_retries_but_permanent_refusal_does_not() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let mut connection = InferenceConnection::new(fixture.host.clone());
    connection.hello();
    connection
        .handle(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::Unavailable {
                reason: RunnerInferenceRejection::StorageUnavailable,
            },
        })
        .await
        .unwrap();
    assert!(connection.poll().await.unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(matches!(
        connection.poll().await.unwrap().as_slice(),
        [EdgeClientMessage::InferenceHello { .. }]
    ));
    connection
        .handle(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::Unavailable {
                reason: RunnerInferenceRejection::ProtocolVersionUnsupported,
            },
        })
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(connection.poll().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn transient_publication_rejection_replays_exact_operation_after_delay() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let mut connection = InferenceConnection::new(fixture.host.clone());
    connection.hello();
    let sent = connection
        .handle(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::accepted(
                RUNNER_INFERENCE_PROTOCOL_VERSION,
                7,
                1_000_000,
            ),
        })
        .await
        .unwrap();
    let publication = sent
        .into_iter()
        .find_map(|message| match message {
            EdgeClientMessage::InferenceBindingPublish { publication } => Some(publication),
            _ => None,
        })
        .unwrap();
    connection
        .handle(EdgeServerMessage::InferenceBindingRejected {
            rejection: RunnerInferenceBindingRejection {
                operation_id: publication.operation_id.clone(),
                reason: RunnerInferenceRejection::StorageUnavailable,
            },
        })
        .await
        .unwrap();
    assert!(connection.poll().await.unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(5)).await;
    let resent = connection.poll().await.unwrap();
    assert!(
        matches!(resent.as_slice(), [EdgeClientMessage::InferenceBindingPublish { publication: replay }] if **replay == *publication)
    );
    connection
        .handle(EdgeServerMessage::InferenceBindingRejected {
            rejection: RunnerInferenceBindingRejection {
                operation_id: publication.operation_id.clone(),
                reason: RunnerInferenceRejection::PublicationConflict,
            },
        })
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(connection.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn connection_accepts_the_servers_full_transfer_window() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let mut connection = InferenceConnection::new(fixture.host.clone());
    connection.hello();
    connection
        .handle(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::accepted(
                RUNNER_INFERENCE_PROTOCOL_VERSION,
                7,
                1_000_000,
            ),
        })
        .await
        .unwrap();
    for index in 0..RUNNER_INFERENCE_MAX_TRANSFERS {
        let grant = fixture.grant(&format!("window-{index}"), REQUEST).await;
        let messages = connection
            .handle(EdgeServerMessage::InferenceDispatch {
                grant: Box::new(grant),
                delivery_generation: 7,
            })
            .await
            .unwrap();
        assert!(matches!(
            messages.as_slice(),
            [EdgeClientMessage::InferenceRequestCredit { .. }]
        ));
    }
}

#[tokio::test]
async fn expired_queued_grant_reconciliation_seals_only_current_incarnation_no_start() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let mut connection = InferenceConnection::new(fixture.host.clone());
    connection.hello();
    connection
        .handle(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::accepted(
                RUNNER_INFERENCE_PROTOCOL_VERSION,
                7,
                1_000_000,
            ),
        })
        .await
        .unwrap();
    let mut grant = fixture.grant("expired-queued", REQUEST).await;
    grant.start_before_unix_ms = 999_999;
    let messages = connection
        .handle(EdgeServerMessage::InferenceReconcile {
            grant: Box::new(grant.clone()),
            delivery_generation: 7,
        })
        .await
        .unwrap();
    assert!(matches!(
        messages.as_slice(),
        [EdgeClientMessage::InferenceStartEvidence {
            evidence: RunnerInferenceStartEvidence::ExpiredWithoutFence,
            ..
        }]
    ));
    assert!(matches!(
        fixture.host.reconcile(&grant).await.unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::ExpiredWithoutFence)
    ));
    let mut replaced = fixture.grant("expired-old-boot", REQUEST).await;
    replaced.start_before_unix_ms = 999_999;
    replaced.process_boot_nonce = id("old-boot");
    assert!(
        connection
            .handle(EdgeServerMessage::InferenceReconcile {
                grant: Box::new(replaced.clone()),
                delivery_generation: 7,
            })
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        fixture.host.reconcile(&replaced).await.unwrap(),
        DispatchOutcome::Unknown
    ));
}

#[tokio::test]
async fn standalone_environment_snapshot_revokes_missing_credentials() {
    let reference = LocalCredentialRef::Environment {
        name: "TEST_KEY".into(),
    };
    let fixture = Fixture::new_with_credential("http://127.0.0.1:9", reference.clone()).await;
    assert!(fixture.host.bindings().await.unwrap().is_empty());
    let config = fixture.host.config().await.unwrap();
    let key = ResolvedLocalCredential::from_environment(&reference, |_| Some("test-key".into()))
        .unwrap()
        .unwrap();
    fixture
        .host
        .refresh_environment(config.revision, vec![("local".into(), 1, key)])
        .await
        .unwrap();
    assert_eq!(fixture.host.bindings().await.unwrap().len(), 1);
    assert!(
        fixture
            .host
            .refresh_environment(config.revision + 1, Vec::new())
            .await
            .is_err()
    );
    assert_eq!(fixture.host.bindings().await.unwrap().len(), 1);
    let mut changed = config.clone();
    changed.models.get_mut("local").unwrap().credential = LocalCredentialRef::Environment {
        name: "NEW_KEY".into(),
    };
    let replaced = LocalModelConfigStore::with_path(fixture.directory.path().join("models.json"))
        .replace(config.revision, changed)
        .unwrap();
    assert!(
        fixture.host.bindings().await.unwrap().is_empty(),
        "a changed credential must receive a new generation and cannot reuse an old slot"
    );
    fixture
        .host
        .refresh_environment(replaced.revision, Vec::new())
        .await
        .unwrap();
    assert!(fixture.host.bindings().await.unwrap().is_empty());
}

#[tokio::test]
async fn request_credit_generation_and_offset_gate_precede_provider_execution() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(&server.uri()).await;
    let grant = fixture.grant("chunks", REQUEST).await;
    let mut connection = InferenceConnection::new(fixture.host.clone());
    let _hello = connection.hello();
    connection
        .handle(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::Accepted {
                protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
                delivery_generation: 7,
                max_artifact_bytes: RUNNER_INFERENCE_ARTIFACT_BYTES as u32,
                server_unix_ms: 1_000_000,
            },
        })
        .await
        .unwrap();
    assert_eq!(
        connection
            .handle(EdgeServerMessage::InferenceDispatch {
                grant: Box::new(grant.clone()),
                delivery_generation: 6
            })
            .await
            .unwrap_err(),
        InferenceHostError::WrongIncarnation
    );
    let credit = connection
        .handle(EdgeServerMessage::InferenceDispatch {
            grant: Box::new(grant.clone()),
            delivery_generation: 7,
        })
        .await
        .unwrap();
    assert!(matches!(
        credit.as_slice(),
        [EdgeClientMessage::InferenceRequestCredit { next_offset: 0, .. }]
    ));
    let bad = EdgeServerMessage::InferenceRequestChunk {
        attempt_id: grant.attempt.attempt_id.clone(),
        delivery_generation: 7,
        chunk: RunnerInferencePayloadChunk {
            offset: 1,
            data: RunnerInferenceChunkData::new(REQUEST.into()).unwrap(),
        },
    };
    assert_eq!(
        connection.handle(bad).await.unwrap_err(),
        InferenceHostError::InvalidRequest
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(matches!(
        fixture.host.reconcile(&grant).await.unwrap(),
        DispatchOutcome::Unknown
    ));
}

#[tokio::test]
async fn runner_preview_connection_sends_first_event_and_drops_duplicate_sequences() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let grant = fixture.grant("preview-connection", REQUEST).await;
    let mut connection = InferenceConnection::new(fixture.host.clone());
    fixture
        .host
        .set_attempt_active_for_test(&grant.attempt, true)
        .await;
    connection.hello();
    connection
        .handle(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::Accepted {
                protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
                delivery_generation: 7,
                max_artifact_bytes: RUNNER_INFERENCE_ARTIFACT_BYTES as u32,
                server_unix_ms: 1_000_000,
            },
        })
        .await
        .unwrap();

    let first = connection
        .handle_preview(InferencePreview {
            attempt: grant.attempt.clone(),
            sequence: 0,
            event: RunnerInferenceProviderEvent::Json(serde_json::json!({
                "choices": [{"delta": {"content": "first"}}]
            })),
        })
        .await
        .unwrap();
    assert!(matches!(
        first.as_slice(),
        [EdgeClientMessage::InferenceProgress { progress, delivery_generation }]
            if *delivery_generation == 7
                && progress.events.len() == 1
                && progress.events[0].sequence == 0
    ));

    // The first event is no longer in the pending batch, so the duplicate is
    // still rejected by the retained sequence watermark rather than echoed.
    assert!(
        connection
            .handle_preview(InferencePreview {
                attempt: grant.attempt.clone(),
                sequence: 0,
                event: RunnerInferenceProviderEvent::Json(serde_json::json!({
                    "choices": [{"delta": {"content": "first"}}]
                })),
            })
            .await
            .unwrap()
            .is_empty()
    );

    let terminal_batch = connection
        .handle_preview(InferencePreview {
            attempt: grant.attempt,
            sequence: 1,
            event: RunnerInferenceProviderEvent::Done,
        })
        .await
        .unwrap();
    assert!(matches!(
        terminal_batch.as_slice(),
        [EdgeClientMessage::InferenceProgress { progress, .. }]
            if progress.events.len() == 1
                && progress.events[0].sequence == 1
                && matches!(progress.events[0].event, RunnerInferenceProviderEvent::Done)
    ));
}

#[tokio::test]
async fn runner_preview_assemblies_are_retired_when_provider_ends_without_done_marker() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let mut connection = InferenceConnection::new(fixture.host.clone());
    let mut completed_attempts = Vec::new();
    connection.hello();
    connection
        .handle(EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::Accepted {
                protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
                delivery_generation: 7,
                max_artifact_bytes: RUNNER_INFERENCE_ARTIFACT_BYTES as u32,
                server_unix_ms: 1_000_000,
            },
        })
        .await
        .unwrap();

    for index in 0..MAX_PENDING_PROGRESS_FOR_TEST {
        let grant = fixture
            .grant(&format!("failed-preview-{index}"), REQUEST)
            .await;
        fixture
            .host
            .set_attempt_active_for_test(&grant.attempt, true)
            .await;
        assert!(
            !connection
                .handle_preview(InferencePreview {
                    attempt: grant.attempt.clone(),
                    sequence: 0,
                    event: RunnerInferenceProviderEvent::Json(serde_json::json!({
                        "choices": [{"delta": {"content": "partial"}}]
                    })),
                })
                .await
                .unwrap()
                .is_empty()
        );
        let response = RunnerInferenceResponse {
            events: vec![RunnerInferenceProviderEvent::Json(serde_json::json!({
                "choices": [{"delta": {"content": "partial"}}]
            }))],
            transport: RunnerInferenceTransportTerminal {
                status: RunnerInferenceTransportStatus::Transport,
                delivery: RunnerInferenceDeliveryEvidence::MayHaveDispatched,
                provider_bytes: 1,
                events_delivered: 1,
            },
        };
        let payload = RetainedTerminal::new(
            physical_terminal(&response),
            serde_json::to_string(&response).unwrap(),
        )
        .unwrap();
        let stored = grant.clone();
        fixture
            .host
            .with_journal(move |journal| {
                assert!(journal.fence(&stored)?);
                journal.complete(&stored, payload)
            })
            .await
            .unwrap();
        fixture
            .host
            .set_attempt_active_for_test(&grant.attempt, false)
            .await;
        completed_attempts.push(grant.attempt);
    }

    assert_eq!(
        connection.pending_progress_len_for_test(),
        MAX_PENDING_PROGRESS_FOR_TEST
    );
    let messages = connection.poll().await.unwrap();
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, EdgeClientMessage::InferenceTerminal { .. }))
    );
    assert!(
        connection.pending_progress_len_for_test() == 0,
        "terminal polling must retire failed streams even without Done/Eof"
    );
    let delayed = connection
        .handle_preview(InferencePreview {
            attempt: completed_attempts[0].clone(),
            sequence: 1,
            event: RunnerInferenceProviderEvent::Json(serde_json::json!({
                "choices": [{"delta": {"content": "late"}}]
            })),
        })
        .await
        .unwrap();
    assert!(delayed.is_empty());
    assert_eq!(
        connection.pending_progress_len_for_test(),
        0,
        "a preview queued behind terminal custody must not reopen its assembly"
    );
}

#[tokio::test]
async fn cancellation_after_provider_acceptance_retains_unknown_and_never_redispatches() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(&server.uri()).await;
    let accepted = Arc::new(Notify::new());
    let notify = accepted.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            notify.notify_one();
            ResponseTemplate::new(200).set_delay(Duration::from_secs(3600))
        })
        .expect(1)
        .mount(&server)
        .await;
    let grant = fixture.grant("after-start", REQUEST).await;
    fixture
        .host
        .dispatch(grant.clone(), REQUEST.into(), fixture.clock)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), accepted.notified())
        .await
        .unwrap();
    fixture.host.cancel(&grant).await.unwrap();
    let payload = fixture.terminal().await;
    let response: RunnerInferenceResponse = serde_json::from_str(&payload.response_json).unwrap();
    assert_eq!(
        response.transport.status,
        RunnerInferenceTransportStatus::Cancelled
    );
    assert_ne!(
        response.transport.delivery,
        RunnerInferenceDeliveryEvidence::NotDispatched
    );
    assert_eq!(
        payload.terminal.status,
        InferenceTerminalStatus::DeliveryUnknown
    );
    assert!(matches!(
        fixture
            .host
            .dispatch(grant, REQUEST.into(), fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::Terminal(_)
    ));
}

#[tokio::test]
async fn partial_provider_transport_failure_retains_ordered_native_event_evidence() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(&format!("http://{}", listener.local_addr().unwrap())).await;
    let provider = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&chunk[..count]);
            if request
                .windows(REQUEST.len())
                .any(|bytes| bytes == REQUEST.as_bytes())
            {
                break;
            }
        }
        let event =
            "data: {\"id\":\"partial\",\"choices\":[{\"delta\":{\"content\":\"🦀局部\"}}]}\n\n";
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n", event.len(), event).as_bytes()).await.unwrap();
        // EOF without the terminating HTTP chunk is transport truncation, not
        // a clean SSE EOF. The first complete event must survive it.
        socket.shutdown().await.unwrap();
    });
    let grant = fixture.grant("partial", REQUEST).await;
    fixture
        .host
        .dispatch(grant, REQUEST.into(), fixture.clock)
        .await
        .unwrap();
    let payload = fixture.terminal().await;
    provider.await.unwrap();
    let response: RunnerInferenceResponse = serde_json::from_str(&payload.response_json).unwrap();
    assert_eq!(
        response.transport.status,
        RunnerInferenceTransportStatus::Transport
    );
    assert!(response.transport.provider_bytes > 0);
    assert_eq!(response.events.len(), 1);
    let RunnerInferenceProviderEvent::Json(value) = &response.events[0] else {
        panic!("native event missing")
    };
    assert_eq!(value["choices"][0]["delta"]["content"], "🦀局部");
    assert_eq!(
        payload.terminal.status,
        InferenceTerminalStatus::DeliveryUnknown
    );
}

#[tokio::test]
async fn utf8_response_credit_is_cumulative_and_reconnect_replays_identical_artifact() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    let grant = fixture.grant("unicode", REQUEST).await;
    let response = RunnerInferenceResponse {
        events: vec![
            RunnerInferenceProviderEvent::Json(
                serde_json::json!({"choices":[{"delta":{"content":"🦀汉".repeat(110_000)}}]}),
            ),
            RunnerInferenceProviderEvent::Done,
        ],
        transport: RunnerInferenceTransportTerminal {
            status: RunnerInferenceTransportStatus::Complete,
            delivery: RunnerInferenceDeliveryEvidence::ResponseHeaders,
            provider_bytes: 770_000,
            events_delivered: 2,
        },
    };
    let payload = RetainedTerminal::new(
        physical_terminal(&response),
        serde_json::to_string(&response).unwrap(),
    )
    .unwrap();
    let expected = payload.response_json.clone();
    let stored = grant.clone();
    fixture
        .host
        .with_journal(move |journal| {
            journal.fence(&stored)?;
            journal.complete(&stored, payload)
        })
        .await
        .unwrap();
    for generation in [1, 2] {
        let mut connection = InferenceConnection::new(fixture.host.clone());
        connection.hello();
        let messages = connection
            .handle(EdgeServerMessage::InferenceHelloAck {
                negotiation: RunnerInferenceNegotiation::Accepted {
                    protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
                    delivery_generation: generation,
                    max_artifact_bytes: RUNNER_INFERENCE_ARTIFACT_BYTES as u32,
                    server_unix_ms: 1_000_000,
                },
            })
            .await
            .unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, EdgeClientMessage::InferenceTerminal { .. }))
                .count(),
            1
        );
        let mut reconstructed = String::new();
        while reconstructed.len() < expected.len() {
            // An early replenishment can arrive after the preceding window's
            // final frame. It extends credit without rewinding sent bytes.
            let mut next_offset = reconstructed.len().saturating_sub(19);
            while !expected.is_char_boundary(next_offset) {
                next_offset -= 1;
            }
            let messages = connection
                .handle(EdgeServerMessage::InferenceResponseCredit {
                    attempt_id: grant.attempt.attempt_id.clone(),
                    delivery_generation: generation,
                    next_offset: next_offset as u32,
                    credit_bytes: (RUNNER_INFERENCE_CHUNK_BYTES * 8) as u32,
                })
                .await
                .unwrap();
            assert!(
                !messages.is_empty(),
                "overlapping credit must make progress"
            );
            assert!(messages.len() <= 9);
            for message in messages {
                let EdgeClientMessage::InferenceResponseChunk { chunk, .. } = message else {
                    panic!("expected bounded chunk")
                };
                assert_eq!(chunk.offset as usize, reconstructed.len());
                assert!(chunk.data.as_bytes().len() <= RUNNER_INFERENCE_CHUNK_BYTES);
                reconstructed.push_str(std::str::from_utf8(chunk.data.as_bytes()).unwrap());
            }
        }
        assert_eq!(reconstructed, expected);
    }
    assert_eq!(
        fixture.host.pending(1).await.unwrap().len(),
        1,
        "credit is not custody ACK"
    );
}
