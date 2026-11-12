use super::*;
use astra_credentials::{
    LocalChatgptModel, LocalModelScope,
    chatgpt::{Registration, Store, Tokens},
    native::{Environment, NativeSession, NativeStore},
    native_device::NativeDeviceAttachment,
};
use wiremock::matchers::{header, path};

struct NativeFixture {
    fixture: Fixture,
    native: NativeStore,
    session: NativeSession,
    scope: LocalModelScope,
    connection: uuid::Uuid,
    _credentials: astra_credentials::CredentialsDirOverrideGuard,
}

impl NativeFixture {
    async fn new(platform: &MockServer, provider: &MockServer) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let guard =
            astra_credentials::set_test_credentials_dir(directory.path().join("credentials"));
        let native = NativeStore::with_directory(directory.path().join("native"));
        let origin = platform.uri();
        let session = native
            .publish(NativeSession {
                environment: Environment {
                    issuer: origin.clone(),
                    astra_url: origin.clone(),
                    moi_url: format!("{origin}/newmoi"),
                    authorization_endpoint: format!("{origin}/protocol/openid-connect/auth"),
                    token_endpoint: format!("{origin}/protocol/openid-connect/token"),
                    revocation_endpoint: format!("{origin}/protocol/openid-connect/revoke"),
                    jwks_uri: format!("{origin}/protocol/openid-connect/certs"),
                },
                generation: String::new(),
                subject: "fixture-principal".into(),
                session_id: "fixture-uc-session".into(),
                astra_user_id: "fixture-user".into(),
                moi_principal_id: "fixture-moi-user".into(),
                catalog_user_id: "fixture-catalog-user".into(),
                access_token: "synthetic-platform-access".into(),
                refresh_token: "synthetic-platform-refresh".into(),
                expires_at: astra_credentials::native::unix_now().unwrap() + 3600,
                workspace_id: Some("fixture-workspace".into()),
                role_id: None,
                refresh_pending: false,
            })
            .unwrap()
            .0;
        native
            .select_workspace(&session, Some("fixture-workspace"))
            .unwrap();
        let session = native.current().unwrap();
        let scope = LocalModelScope::for_native_session(&session).unwrap();
        let credentials = Store::new(&scope);
        let connection = uuid::Uuid::new_v4();
        credentials
            .authorize(
                connection,
                None,
                "Synthetic account".into(),
                Registration {
                    client_id: "synthetic-issued-client".into(),
                    subject: "synthetic-provider-user".into(),
                    host_id: credentials.host_id().unwrap(),
                },
                Tokens::new(
                    "synthetic-provider-access".into(),
                    "synthetic-provider-refresh".into(),
                    astra_credentials::native::unix_now().unwrap() + 3600,
                )
                .unwrap(),
                true,
            )
            .unwrap();
        let mut config = LocalModelConfig::default();
        config.chatgpt_models.insert(
            "local-chatgpt".into(),
            LocalChatgptModel {
                connection_ref: connection,
                authorization_revision: 1,
                model: "fixture".into(),
                binding_revision: 1,
                context_window: 1024,
                max_output_tokens: 64,
                supported_reasoning_efforts: vec!["high".into()],
            },
        );
        scope.models().replace(0, config).unwrap();
        let mut host = InferenceHost::open(
            scope.root().join("journal"),
            InferenceOwner {
                deployment_identity: scope.identity().into(),
                user_id: "fixture-user".into(),
                runner_id: id("fixture-runner"),
            },
            scope.models().path().into(),
            scope.root().join("secrets"),
            transport(),
            Some(native.clone()),
        )
        .await
        .unwrap();
        Arc::get_mut(&mut host).unwrap().chatgpt =
            Some(crate::chatgpt::Client::with_test_origin(&provider.uri()));
        host.enable_managed().await;
        host.attach_client("terminal-a".into()).await.unwrap();
        host.attach_native_client(
            "terminal-a",
            NativeDeviceAttachment::new(&native, &session).unwrap(),
        )
        .await
        .unwrap();
        let now = Instant::now();
        Self {
            fixture: Fixture {
                directory,
                host,
                clock: GrantClock::observed(1_000_000, now, now).unwrap(),
            },
            native,
            session,
            scope,
            connection,
            _credentials: guard,
        }
    }

    fn enable(&self) {
        self.native
            .enable_device_connection(&self.session, self.connection, 1)
            .unwrap();
    }
}

const RESPONSES_REQUEST: &str = r#"{"model":"fixture","input":[{"role":"user","content":"synthetic prompt"}],"max_output_tokens":64,"stream":true,"store":false}"#;

#[tokio::test]
async fn native_device_publication_requires_explicit_current_session_consent() {
    let platform = MockServer::start().await;
    let provider = MockServer::start().await;
    let fixture = NativeFixture::new(&platform, &provider).await;
    assert!(fixture.fixture.host.bindings().await.unwrap().is_empty());
    fixture.enable();
    let before = fixture.fixture.host.bindings().await.unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].protocol, RunnerInferenceProtocol::OpenAiResponses);
    fixture.native.logout().unwrap();
    assert!(fixture.fixture.host.bindings().await.unwrap().is_empty());
    assert!(
        fixture
            .fixture
            .host
            .native_connection_owner()
            .await
            .is_err()
    );
    assert!(
        Store::new(&fixture.scope)
            .token_snapshot(fixture.connection)
            .is_ok()
    );
    assert!(provider.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn shared_native_host_reconnect_uses_an_independent_still_authorized_terminal() {
    let platform = MockServer::start().await;
    let provider = MockServer::start().await;
    let fixture = NativeFixture::new(&platform, &provider).await;
    fixture.enable();
    let second =
        NativeStore::with_directory(fixture.fixture.directory.path().join("second-native"));
    let mut next = fixture.session.clone();
    next.session_id = "independent-uc-session".into();
    let next = second.publish(next).unwrap().0;
    second
        .select_workspace(&next, Some("fixture-workspace"))
        .unwrap();
    let next = second.current().unwrap();
    second
        .enable_device_connection(&next, fixture.connection, 1)
        .unwrap();
    fixture
        .fixture
        .host
        .attach_client("terminal-b".into())
        .await
        .unwrap();
    fixture
        .fixture
        .host
        .attach_native_client(
            "terminal-b",
            NativeDeviceAttachment::new(&second, &next).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fixture.fixture.host.bindings().await.unwrap().len(), 2);
    fixture.native.logout().unwrap();
    fixture.fixture.host.detach_client("terminal-a").await;
    let (_, owner) = fixture
        .fixture
        .host
        .native_connection_owner()
        .await
        .unwrap();
    assert_eq!(owner.session_id, "independent-uc-session");
    let bindings = fixture.fixture.host.bindings().await.unwrap();
    assert_eq!(bindings.len(), 1);
    assert!(
        matches!(&bindings[0].source, RunnerInferenceSource::Chatgpt { native_context, .. }
        if native_context.session_id.as_str() == "independent-uc-session")
    );
}

#[tokio::test]
async fn native_responses_dispatch_reconciles_terminal_without_replaying_provider_request() {
    let platform = MockServer::start().await;
    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/auth/me"))
        .and(header("authorization", "Bearer synthetic-platform-access"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"user_id":"fixture-user"})),
        )
        .expect(1)
        .mount(&platform)
        .await;
    let completion = serde_json::json!({"type":"response.completed","response":{"id":"synthetic-response","status":"completed",
        "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Synthetic answer"}]}],
        "usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer synthetic-provider-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!("event: response.completed\ndata: {completion}\n\n"))
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&provider)
        .await;
    let fixture = NativeFixture::new(&platform, &provider).await;
    fixture.enable();
    let grant = fixture
        .fixture
        .grant("native-success", RESPONSES_REQUEST)
        .await;
    assert!(matches!(
        fixture
            .fixture
            .host
            .dispatch(
                grant.clone(),
                RESPONSES_REQUEST.into(),
                fixture.fixture.clock
            )
            .await
            .unwrap(),
        DispatchOutcome::Started
    ));
    let terminal = fixture.fixture.terminal().await;
    let response: RunnerInferenceResponse = serde_json::from_str(&terminal.response_json).unwrap();
    assert_eq!(
        response.transport.delivery,
        RunnerInferenceDeliveryEvidence::ResponseHeaders
    );
    assert_eq!(
        response.transport.status,
        RunnerInferenceTransportStatus::Complete
    );
    assert!(!response.events.is_empty());
    assert!(matches!(
        fixture
            .fixture
            .host
            .dispatch(grant, RESPONSES_REQUEST.into(), fixture.fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::Terminal(_)
    ));
    assert_eq!(provider.received_requests().await.unwrap().len(), 1);
    assert!(!terminal.response_json.contains("synthetic-provider-access"));
    assert!(!terminal.response_json.contains("synthetic-platform-access"));
}

#[tokio::test]
async fn remotely_revoked_platform_session_never_starts_openai_request() {
    let platform = MockServer::start().await;
    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/auth/me"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&platform)
        .await;
    let fixture = NativeFixture::new(&platform, &provider).await;
    fixture.enable();
    let grant = fixture
        .fixture
        .grant("revoked-platform", RESPONSES_REQUEST)
        .await;
    assert!(matches!(
        fixture
            .fixture
            .host
            .dispatch(grant, RESPONSES_REQUEST.into(), fixture.fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::NotStarted(RunnerInferenceStartEvidence::RejectedWithoutFence)
    ));
    let terminal = fixture.fixture.terminal().await;
    let response: RunnerInferenceResponse = serde_json::from_str(&terminal.response_json).unwrap();
    assert_eq!(
        response.transport.status,
        RunnerInferenceTransportStatus::CredentialUnavailable
    );
    assert!(provider.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn logout_and_terminal_detach_do_not_wait_for_platform_io_or_dispatch_after_it() {
    // This deliberately slow network case must not hold the process-wide
    // credential override while unrelated tests queue for its short guard.
    const CHILD: &str = "ASTRA_TEST_NATIVE_NETWORK_FENCE";
    if std::env::var(CHILD).as_deref() != Ok("1") {
        let status = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "inference_host::tests::native_inference_tests::logout_and_terminal_detach_do_not_wait_for_platform_io_or_dispatch_after_it", "--nocapture"])
            .env(CHILD, "1").status().await.unwrap();
        assert!(status.success());
        return;
    }
    for logout in [false, true] {
        let platform = MockServer::start().await;
        let provider = MockServer::start().await;
        let reached = Arc::new(tokio::sync::Notify::new());
        let signal = reached.clone();
        Mock::given(method("GET"))
            .and(path("/auth/me"))
            .respond_with(move |_: &wiremock::Request| {
                signal.notify_one();
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"user_id":"fixture-user"}))
                    .set_delay(Duration::from_secs(2))
            })
            .expect(1)
            .mount(&platform)
            .await;
        let fixture = NativeFixture::new(&platform, &provider).await;
        fixture.enable();
        let grant = fixture
            .fixture
            .grant("changing-authorization", RESPONSES_REQUEST)
            .await;
        let host = fixture.fixture.host.clone();
        let clock = fixture.fixture.clock;
        let dispatch =
            tokio::spawn(
                async move { host.dispatch(grant, RESPONSES_REQUEST.into(), clock).await },
            );
        tokio::time::timeout(Duration::from_secs(2), reached.notified())
            .await
            .unwrap();
        if logout {
            let native = fixture.native.clone();
            tokio::time::timeout(
                Duration::from_millis(500),
                tokio::task::spawn_blocking(move || native.logout()),
            )
            .await
            .expect("logout must not wait for network I/O")
            .unwrap()
            .unwrap();
        } else {
            tokio::time::timeout(
                Duration::from_millis(500),
                fixture.fixture.host.detach_client("terminal-a"),
            )
            .await
            .expect("terminal detach must not wait for network I/O");
        }
        assert!(matches!(
            dispatch.await.unwrap().unwrap(),
            DispatchOutcome::NotStarted(_)
        ));
        assert!(provider.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn native_republication_retains_revision_watermarks_after_workspace_roundtrip() {
    let platform = MockServer::start().await;
    let provider = MockServer::start().await;
    let fixture = NativeFixture::new(&platform, &provider).await;
    fixture.enable();
    let host = &fixture.fixture.host;
    let first = host.next_publication().await.unwrap().unwrap();
    let original = first.change.identity().clone();
    assert!(original.binding_id.as_str().starts_with("native-"));
    host.publication_ack(RunnerInferenceBindingReceipt {
        operation_id: first.operation_id,
        publication_revision: NonZeroU64::new(1).unwrap(),
        identity: original.clone(),
    })
    .await
    .unwrap();
    fixture
        .native
        .select_workspace(&fixture.session, Some("workspace-b"))
        .unwrap();
    let disabled = host.next_publication().await.unwrap().unwrap();
    assert!(matches!(
        disabled.change,
        RunnerInferenceBindingChange::Disable { .. }
    ));
    host.publication_ack(RunnerInferenceBindingReceipt {
        operation_id: disabled.operation_id,
        publication_revision: NonZeroU64::new(2).unwrap(),
        identity: original.clone(),
    })
    .await
    .unwrap();
    let b = fixture.native.current().unwrap();
    fixture
        .native
        .select_workspace(&b, Some("fixture-workspace"))
        .unwrap();
    assert!(
        host.bindings().await.unwrap().is_empty(),
        "returning to a workspace cannot restore old consent"
    );
    let returned = fixture.native.current().unwrap();
    fixture
        .native
        .enable_device_connection(&returned, fixture.connection, 1)
        .unwrap();
    let restored = host.next_publication().await.unwrap().unwrap();
    let identity = restored.change.identity();
    assert_eq!(identity.binding_id, original.binding_id);
    assert_eq!(
        identity.binding_revision.get(),
        2,
        "server expects the next durable binding revision"
    );
    assert_eq!(restored.expected_publication_revision, 2);
    assert!(provider.received_requests().await.unwrap().is_empty());
}
