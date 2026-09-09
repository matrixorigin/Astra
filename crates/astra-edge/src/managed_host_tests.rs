use super::*;

fn credential(value: &str) -> ResolvedLocalCredential {
    ResolvedLocalCredential::from_environment(
        &LocalCredentialRef::Environment {
            name: "CANARY".into(),
        },
        |_| Some(value.to_owned()),
    )
    .unwrap()
    .unwrap()
}

async fn acknowledge_publications(host: &Arc<InferenceHost>) {
    for _ in 0..1024 {
        let Some(publication) = host.next_publication().await.unwrap() else {
            return;
        };
        host.publication_ack(RunnerInferenceBindingReceipt {
            operation_id: publication.operation_id,
            publication_revision: NonZeroU64::new(publication.expected_publication_revision + 1)
                .unwrap(),
            identity: publication.change.identity().clone(),
        })
        .await
        .unwrap();
    }
    panic!("publication did not converge");
}

#[tokio::test]
async fn managed_terminal_credentials_are_independent_and_detach_does_not_cancel() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(100))
                .set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                ),
        )
        .expect(2)
        .mount(&server)
        .await;
    let fixture = Fixture::new_with_credential(
        &server.uri(),
        LocalCredentialRef::Environment {
            name: "CANARY".into(),
        },
    )
    .await;
    fixture.host.enable_managed().await;
    assert!(fixture.host.bindings().await.unwrap().is_empty());
    for (client, key) in [
        ("terminal-a", "synthetic-canary-a"),
        ("terminal-b", "synthetic-canary-b"),
    ] {
        fixture.host.attach_client(client.into()).await.unwrap();
        fixture
            .host
            .refresh_client(client, 1, vec![("local".into(), 1, credential(key))])
            .await
            .unwrap();
    }
    acknowledge_publications(&fixture.host).await;
    let definitions = fixture.host.bindings().await.unwrap();
    assert_eq!(definitions.len(), 2);
    let mut grants = Vec::new();
    for client in ["terminal-a", "terminal-b"] {
        let mut grant = fixture.grant(client, REQUEST).await;
        grant.attempt.binding = definitions
            .iter()
            .find(|definition| {
                definition.identity.binding_id.as_str() == local_binding_id("local", Some(client))
            })
            .unwrap()
            .identity
            .clone();
        assert!(matches!(
            fixture
                .host
                .dispatch(grant.clone(), REQUEST.into(), fixture.clock)
                .await
                .unwrap(),
            DispatchOutcome::Started
        ));
        grants.push(grant);
    }
    fixture.host.detach_client("terminal-a").await;
    assert_eq!(fixture.host.bindings().await.unwrap().len(), 1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.host.active_count().await != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        fixture
            .host
            .pending(8)
            .await
            .unwrap()
            .iter()
            .all(|(_, terminal)| terminal.terminal.status == InferenceTerminalStatus::Succeeded)
    );
    let requests = server.received_requests().await.unwrap();
    let mut keys = requests
        .iter()
        .map(|request| {
            request
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        ["Bearer synthetic-canary-a", "Bearer synthetic-canary-b"]
    );
    for grant in &grants {
        let path = fixture.directory.path().join("journal").join(format!(
            "attempt-{}.json",
            grant.attempt.attempt_id.as_str()
        ));
        let retained = std::fs::read_to_string(path).unwrap();
        assert!(!retained.contains("synthetic-canary-a"));
        assert!(!retained.contains("synthetic-canary-b"));
    }
    // A newly granted request targeting the departed terminal cannot borrow B.
    let mut departed = grants.remove(0);
    departed.attempt.attempt_id = id("departed-new");
    departed.attempt.invocation_id = id("departed-invocation");
    departed.grant_id = id("departed-new");
    assert!(matches!(
        fixture
            .host
            .dispatch(departed, REQUEST.into(), fixture.clock)
            .await
            .unwrap(),
        DispatchOutcome::NotStarted(_)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn managed_environment_retirement_bounds_local_catalog_across_repeated_launches() {
    let fixture = Fixture::new_with_credential(
        "http://127.0.0.1:9",
        LocalCredentialRef::Environment {
            name: "CANARY".into(),
        },
    )
    .await;
    fixture.host.enable_managed().await;
    for launch in 0..300 {
        let client = format!("terminal-{launch}");
        fixture.host.attach_client(client.clone()).await.unwrap();
        fixture
            .host
            .refresh_client(
                &client,
                1,
                vec![("local".into(), 1, credential("synthetic"))],
            )
            .await
            .unwrap();
        acknowledge_publications(&fixture.host).await;
        assert_eq!(fixture.host.bindings().await.unwrap().len(), 1);
        fixture.host.detach_client(&client).await;
        acknowledge_publications(&fixture.host).await;
        assert!(fixture.host.journal.lock().unwrap().published().is_empty());
    }
    assert!(fixture.host.bindings().await.unwrap().is_empty());
}

#[tokio::test]
async fn managed_stored_binding_keeps_identity_across_host_restart_and_recovers_custody() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = Fixture::new(&server.uri()).await;
    fixture.host.enable_managed().await;
    fixture.host.attach_client("first".into()).await.unwrap();
    acknowledge_publications(&fixture.host).await;
    let before = fixture.host.bindings().await.unwrap().remove(0).identity;
    let grant = fixture.grant("stored-recovery", REQUEST).await;
    fixture
        .host
        .dispatch(grant.clone(), REQUEST.into(), fixture.clock)
        .await
        .unwrap();
    let terminal = fixture.terminal().await;
    while fixture.host.active_count().await != 0 {
        tokio::task::yield_now().await;
    }
    fixture.host.detach_client("first").await;
    acknowledge_publications(&fixture.host).await;
    let root = fixture.directory.path().to_owned();
    drop(fixture.host);
    let reopened = InferenceHost::open(
        root.join("journal"),
        owner(),
        root.join("models.json"),
        root.join("secrets"),
        transport(),
    )
    .await
    .unwrap();
    reopened.enable_managed().await;
    reopened.attach_client("second".into()).await.unwrap();
    let after = reopened.bindings().await.unwrap().remove(0).identity;
    assert_eq!(before.runner_id, after.runner_id);
    assert_eq!(before.journal_id, after.journal_id);
    assert_eq!(before.binding_id, after.binding_id);
    match reopened.reconcile(&grant).await.unwrap() {
        DispatchOutcome::Terminal(restored) => {
            assert_eq!(restored.terminal_sha256, terminal.terminal_sha256)
        }
        outcome => panic!("expected retained custody, got {outcome:?}"),
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn managed_attachment_bounds_and_foreign_refresh_fail_closed() {
    let fixture = Fixture::new("http://127.0.0.1:9").await;
    fixture.host.enable_managed().await;
    for index in 0..32 {
        fixture.host.attach_client(index.to_string()).await.unwrap();
    }
    assert_eq!(
        fixture
            .host
            .attach_client("overflow".into())
            .await
            .unwrap_err(),
        InferenceHostError::Capacity
    );
    assert_eq!(
        fixture
            .host
            .refresh_client("foreign", 1, Vec::new())
            .await
            .unwrap_err(),
        InferenceHostError::OwnerMismatch
    );
    fixture.host.detach_client("0").await;
    fixture
        .host
        .attach_client("replacement".into())
        .await
        .unwrap();
}
