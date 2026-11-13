use super::*;
#[allow(dead_code)]
mod fixture {
    use crate as services;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/common/chatgpt_fixture.rs"
    ));
}

fn encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new("isolated-subscription-test-encryption-key").unwrap())
}

#[test]
fn personal_setup_defaults_on_with_stable_key_scoped_identity() {
    let config = || ChatgptConfig::from_settings(None, String::new()).unwrap();
    let key = encryptor();
    let first = ChatgptConnections::new(None, key.clone(), Ok(config()));
    let restarted = ChatgptConnections::new(None, key, Ok(config()));
    assert_eq!(first.availability()["available"], true);
    let host = &first.enabled().unwrap().host_id;
    assert_eq!(host, &restarted.enabled().unwrap().host_id);
    assert!(host.starts_with("urn:uuid:"));
    assert!(Uuid::parse_str(&host[9..]).is_ok());
    let separate = ChatgptConnections::new(
        None,
        Arc::new(FernetTokenEncryptor::new("another-installation-key").unwrap()),
        Ok(config()),
    );
    assert_ne!(host, &separate.enabled().unwrap().host_id);
    let explicit = Uuid::new_v4().urn().to_string();
    let service = ChatgptConnections::new(
        None,
        encryptor(),
        ChatgptConfig::from_settings(None, explicit.clone()),
    );
    assert_eq!(service.enabled().unwrap().host_id, explicit);
    for flag in ["", "1", "true"] {
        assert!(
            ChatgptConfig::from_settings(Some(flag), String::new())
                .unwrap()
                .enabled
        );
    }
    for flag in ["0", "false"] {
        let service = ChatgptConnections::new(
            None,
            encryptor(),
            ChatgptConfig::from_settings(Some(flag), String::new()),
        );
        assert_eq!(service.availability()["available"], false);
        assert_eq!(
            service.availability()["error_code"],
            "chatgpt_subscription_disabled"
        );
    }
    for config in [
        ChatgptConfig::from_settings(Some("unexpected"), String::new()),
        ChatgptConfig::from_settings(None, "not-a-uuid".into()),
    ] {
        assert!(config.is_err());
        let service = ChatgptConnections::new(None, encryptor(), config);
        assert_eq!(
            service.availability()["error_code"],
            "chatgpt_subscription_unconfigured"
        );
    }
}

#[test]
fn callback_policy_accepts_only_the_bound_loopback_path() {
    assert!(validate_redirect("http://127.0.0.1:12345/auth/callback").is_ok());
    for value in [
        "http://localhost:12345/auth/callback",
        "https://example.test/auth/callback",
        "http://127.0.0.1:12345/callback",
        "http://127.0.0.1:12345/auth/callback?code=wrong",
        "http://user@127.0.0.1:12345/auth/callback",
        "http://127.0.0.1/auth/callback",
    ] {
        assert!(validate_redirect(value).is_err(), "{value}");
    }
}
#[tokio::test]
async fn provider_identity_requires_signature_audience_and_nonce() {
    let fixture = fixture::ProviderFixture::start().await;
    let key = encryptor();
    let service = ChatgptConnections::new(
        None,
        key.clone(),
        Ok(ChatgptConfig::for_test(&fixture.base)),
    );
    let nonce = key.keyed_digest("chatgpt-nonce", "expected-nonce");
    let token = fixture.id_token("account-one", "oaiapp_fixture", "expected-nonce");
    assert_eq!(
        service
            .identity(&token, "oaiapp_fixture", Some(&nonce))
            .await
            .unwrap()
            .sub,
        "account-one"
    );
    assert!(
        service
            .identity(&token, "another-client", Some(&nonce))
            .await
            .is_err()
    );
    assert!(
        service
            .identity(
                &fixture.id_token("account-one", "oaiapp_fixture", "wrong"),
                "oaiapp_fixture",
                Some(&nonce)
            )
            .await
            .is_err()
    );
    let mut altered = token.into_bytes();
    let last = altered.len() - 5;
    altered[last] = if altered[last] == b'A' { b'B' } else { b'A' };
    assert!(
        service
            .identity(
                std::str::from_utf8(&altered).unwrap(),
                "oaiapp_fixture",
                Some(&nonce)
            )
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .state
            .discovery_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        fixture
            .state
            .jwks_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}
#[tokio::test]
async fn feature_flag_blocks_linking_before_database_or_provider_io() {
    let mut config = ChatgptConfig::for_test("http://127.0.0.1:1");
    config.enabled = false;
    let service = ChatgptConnections::new(None, encryptor(), Ok(config));
    let failure = service
        .start(
            "owner",
            StartAuthorization {
                retry_authorization_id: None,
                redirect_uri: "http://127.0.0.1:12345/auth/callback".into(),
                label: None,
                connection_id: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(failure.0, StatusCode::SERVICE_UNAVAILABLE);
}
#[test]
fn callback_debug_redacts_values_and_provider_urls_reject_untrusted_origins() {
    let request = CompleteAuthorization {
        state: "sensitive-state".into(),
        code: Some("sensitive-code".into()),
        client_id: None,
        error: None,
    };
    let debug = format!("{request:?}");
    assert!(!debug.contains("sensitive"));
    let config = ChatgptConfig::for_test("http://127.0.0.1:1");
    assert!(
        config
            .provider_url("http://user:secret@127.0.0.1:1/jwks")
            .is_err()
    );
    let mut production = config;
    production.test_endpoints = false;
    for url in [
        "http://auth.openai.com/jwks",
        "https://example.test/jwks",
        "https://auth.openai.com@evil.test/jwks",
    ] {
        assert!(production.provider_url(url).is_err());
    }
}
#[test]
fn model_check_failures_keep_typed_provider_limits() {
    for (reason, status, code) in [
        (
            "subscription_sharing_user_not_eligible",
            StatusCode::FORBIDDEN,
            "subscription_sharing_user_not_eligible",
        ),
        (
            "subscription_sharing_usage_limit_exceeded",
            StatusCode::TOO_MANY_REQUESTS,
            "subscription_sharing_usage_limit_exceeded",
        ),
        (
            "rate_limit_exceeded",
            StatusCode::TOO_MANY_REQUESTS,
            "provider_rate_limited",
        ),
        (
            "subscription_sharing_usage_unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
            "subscription_sharing_usage_unavailable",
        ),
        (
            "context_length_exceeded",
            StatusCode::BAD_REQUEST,
            "context_length_exceeded",
        ),
    ] {
        let (actual, Json(error)) =
            model_check_failure(&ResponsesStreamError::Provider(Some(reason.into())));
        assert_eq!(actual, status, "{reason}");
        assert_eq!(error.error_code.as_deref(), Some(code), "{reason}");
        assert!(
            !error.detail.contains(reason),
            "provider prose must not reach the public detail: {reason}"
        );
    }
    for failure in [
        ResponsesStreamError::Provider(None),
        ResponsesStreamError::Provider(Some("raw-provider-code".into())),
        ResponsesStreamError::Codec("Responses stream ended without response.completed"),
        ResponsesStreamError::Codec("Invalid Responses SSE JSON"),
        ResponsesStreamError::Codec("Unsupported Responses output item"),
    ] {
        let (status, Json(error)) = model_check_failure(&failure);
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{failure:?}");
        assert_eq!(
            error.error_code.as_deref(),
            Some("chatgpt_model_check_failed"),
            "{failure:?}"
        );
        assert_eq!(
            error.detail, "Subscription model check did not complete",
            "{failure:?}"
        );
        assert!(
            !error.detail.contains("raw-provider-code"),
            "upstream code must not reach the public detail"
        );
    }
}

fn service_without_pool() -> Arc<ChatgptConnections> {
    Arc::new(ChatgptConnections::new(
        None,
        encryptor(),
        Ok(ChatgptConfig::for_test("http://127.0.0.1:1")),
    ))
}
fn publish(
    service: &ChatgptConnections,
    pin: &CachePin,
    generation: i64,
    version: i64,
    expires: i64,
) -> CacheVerdict {
    service.adjudicate_candidate(
        pin,
        generation,
        None,
        version,
        Some((format!("fixture-{generation}-{version}"), version, expires)),
    )
}
#[tokio::test]
async fn warm_credentials_bypass_cold_admission_but_not_database_fence() {
    let service = service_without_pool();
    let pin = service
        .pin_token_guard("owner", "warm", 7, None)
        .await
        .unwrap();
    assert!(matches!(
        publish(&service, &pin, 7, 3, unix_now() + 3600),
        CacheVerdict::Serve(_, 3, _)
    ));
    let mut pins = vec![pin];
    for n in 1..MAX_CACHED_TOKENS {
        pins.push(
            service
                .pin_token_guard("owner", &format!("other-{n}"), 7, None)
                .await
                .unwrap(),
        );
    }
    assert_eq!(service.token_producers.available_permits(), 0);
    assert_eq!(
        service
            .access_token("owner", "warm", 7, None)
            .await
            .unwrap()
            .1,
        3
    );
    let binding = ExecutionCredential {
        service: service.clone(),
        owner: "owner".into(),
        connection_id: "warm".into(),
        generation: 7,
        label: "Fixture".into(),
    };
    assert!(
        binding.acquire(None).await.is_err(),
        "warm cache does not grant authority without the DB generation fence"
    );
    drop(pins);
    assert_eq!(
        service.token_producers.available_permits(),
        MAX_CACHED_TOKENS
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_rejection_survives_other_connection_eviction() {
    let service = service_without_pool();
    let pin = service
        .pin_token_guard("owner", "protected", 7, Some(3))
        .await
        .unwrap();
    let mut peers = tokio::task::JoinSet::new();
    for n in 0..MAX_CACHED_TOKENS * 2 {
        let service = service.clone();
        peers.spawn(async move {
            let other = service
                .pin_token_guard("owner", &format!("other-{n}"), 1, None)
                .await
                .unwrap();
            assert!(matches!(
                publish(&service, &other, 1, 1, unix_now() + 3600),
                CacheVerdict::Serve(_, 1, _)
            ));
        });
    }
    while let Some(peer) = peers.join_next().await {
        peer.unwrap();
    }
    assert!(matches!(
        publish(&service, &pin, 7, 3, unix_now() + 3600),
        CacheVerdict::Refresh(Some(3))
    ));
    assert!(matches!(
        publish(&service, &pin, 7, 4, unix_now() + 3600),
        CacheVerdict::Serve(_, 4, _)
    ));
    let cache = service.tokens.lock().unwrap();
    assert!(cache.guards.len() <= MAX_CACHED_TOKENS);
    assert!(cache.entries.len() <= MAX_CACHED_TOKENS);
    assert_eq!(cache.guards[&pin.key].pins, 1);
}
#[tokio::test]
async fn candidate_publication_obeys_newest_rejection_and_cached_winner() {
    let service = service_without_pool();
    let pin = service
        .pin_token_guard("owner", "conn", 7, None)
        .await
        .unwrap();
    assert!(matches!(
        publish(&service, &pin, 7, 4, unix_now() + 3600),
        CacheVerdict::Serve(_, 4, _)
    ));
    assert!(matches!(
        publish(&service, &pin, 7, 3, unix_now() + 3600),
        CacheVerdict::Serve(_, 4, _)
    ));
    assert!(
        service
            .cached_access_token("owner", "conn", 7, Some(4))
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        publish(&service, &pin, 7, 4, unix_now() + 3600),
        CacheVerdict::Refresh(Some(4))
    ));
    assert!(matches!(
        publish(&service, &pin, 7, 3, unix_now() + 3600),
        CacheVerdict::Retry(4)
    ));
    assert!(
        service
            .cached_access_token("owner", "conn", 7, Some(3))
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        publish(&service, &pin, 7, 5, unix_now() + 3600),
        CacheVerdict::Serve(_, 5, _)
    ));
    assert_eq!(
        service
            .access_token("owner", "conn", 7, Some(3))
            .await
            .unwrap()
            .1,
        5
    );
}
#[tokio::test]
async fn invalidated_generation_cannot_republish_or_cancel_a_new_generation() {
    let service = service_without_pool();
    let old = service
        .pin_token_guard("owner", "conn", 7, None)
        .await
        .unwrap();
    let old_cancel = CancellationToken::new();
    service.register_request("owner", "conn", Uuid::new_v4(), 7, old_cancel.clone());
    service.invalidate_local_connection("owner", "conn", 7);
    assert!(old_cancel.is_cancelled());
    assert!(matches!(
        publish(&service, &old, 7, 3, unix_now() + 3600),
        CacheVerdict::Invalidated
    ));
    let new = service
        .pin_token_guard("owner", "conn", 8, None)
        .await
        .unwrap();
    assert!(matches!(
        publish(&service, &new, 8, 1, unix_now() + 3600),
        CacheVerdict::Serve(_, 1, _)
    ));
    let new_cancel = CancellationToken::new();
    service.register_request("owner", "conn", Uuid::new_v4(), 8, new_cancel.clone());
    service.invalidate_local_connection("owner", "conn", 7);
    assert!(!new_cancel.is_cancelled());
    assert_eq!(
        service
            .access_token("owner", "conn", 8, None)
            .await
            .unwrap()
            .1,
        1
    );
    assert!(
        service
            .access_token("owner", "conn", 7, None)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn queued_cancellation_and_last_producer_owner_release_capacity() {
    let service = service_without_pool();
    let mut pins = Vec::new();
    for n in 0..MAX_CACHED_TOKENS {
        pins.push(
            service
                .pin_token_guard("owner", &format!("key-{n}"), 1, None)
                .await
                .unwrap(),
        );
    }
    let queued_service = service.clone();
    let queued = tokio::spawn(async move {
        queued_service
            .pin_token_guard("owner", "queued", 1, None)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!queued.is_finished());
    queued.abort();
    assert!(queued.await.err().unwrap().is_cancelled());
    let worker_owner = Arc::clone(&pins[0]);
    drop(pins);
    assert_eq!(
        service.token_producers.available_permits(),
        MAX_CACHED_TOKENS - 1
    );
    drop(worker_owner);
    assert_eq!(
        service.token_producers.available_permits(),
        MAX_CACHED_TOKENS
    );
    let admitted = service
        .pin_token_guard("owner", "after-cancellation", 1, None)
        .await
        .unwrap();
    assert!(matches!(
        publish(&service, &admitted, 1, 1, unix_now() + 3600),
        CacheVerdict::Serve(_, 1, _)
    ));
}
#[tokio::test]
async fn expired_or_short_lived_candidates_do_not_leave_hot_entries() {
    let service = service_without_pool();
    let pin = service
        .pin_token_guard("owner", "conn", 1, None)
        .await
        .unwrap();
    assert!(matches!(
        publish(&service, &pin, 1, 1, unix_now() - 1),
        CacheVerdict::Refresh(_)
    ));
    assert!(matches!(
        publish(&service, &pin, 1, 2, unix_now() + 30),
        CacheVerdict::Serve(_, 2, _)
    ));
    assert!(
        service
            .cached_access_token("owner", "conn", 1, None)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn monitor_exit_after_stop_fails_closed_but_normal_handoff_restarts() {
    for normal in [false, true] {
        let service = service_without_pool();
        let cancel = CancellationToken::new();
        service.register_request("owner", "connection", Uuid::new_v4(), 7, cancel.clone());
        let id = {
            let active = service.active.lock().unwrap();
            let monitor = active.monitor.as_ref().unwrap();
            monitor.stop.cancel();
            monitor.id
        };
        // A stop signal alone does not prove that the task reached its normal
        // handoff. Aborted/unwound tasks must never spawn a successor in Drop.
        let mut guard = MonitorGuard {
            service: Arc::downgrade(&service),
            id,
            armed: true,
        };
        if normal {
            service.finish_monitor(&mut guard);
        }
        drop(guard);
        let active = service.active.lock().unwrap();
        assert_eq!(cancel.is_cancelled(), !normal);
        if normal {
            let successor = active.monitor.as_ref().unwrap();
            assert_ne!(successor.id, id);
            assert!(!successor.stop.is_cancelled());
        } else {
            assert!(active.monitor.is_none());
            assert!(active.connections.is_empty());
        }
        drop(active);
        service.invalidate_local_connection("owner", "connection", 7);
    }
}

#[test]
fn monitor_observations_fence_entry_identity_owner_and_generation() {
    let old = Uuid::new_v4();
    let current = Uuid::new_v4();
    let mut active = ActiveRegistry::default();
    let mut tokens = Vec::new();
    for owner in ["alice", "bob"] {
        let mut requests = Vec::new();
        for generation in [4, 5, 6] {
            let token = CancellationToken::new();
            tokens.push(token.clone());
            requests.push((Uuid::new_v4(), generation, token));
        }
        active.connections.insert(
            (owner.into(), "same-id".into()),
            ActiveConnection {
                monitor: current,
                requests,
            },
        );
    }
    let alice = ("alice".into(), "same-id".into());
    active.apply_observations(&[(alice.clone(), old)], None);
    assert!(
        tokens.iter().all(|token| !token.is_cancelled()),
        "late old entry cannot cancel its replacement"
    );
    active.apply_observations(
        &[(alice.clone(), current)],
        Some(&[Some(ConnectionRow {
            status: "disconnected".into(),
            generation: 5,
            host_id: String::new(),
            label: String::new(),
        })]),
    );
    assert!(tokens[0].is_cancelled() && tokens[1].is_cancelled());
    assert!(
        !tokens[2].is_cancelled(),
        "older observation cannot cancel a newer generation"
    );
    assert!(
        tokens[3..].iter().all(|token| !token.is_cancelled()),
        "owner isolation"
    );
    active.apply_observations(&[(alice, current)], None);
    assert!(
        tokens[2].is_cancelled(),
        "failed observation fails closed for the matching entry"
    );
}

#[test]
fn registration_during_runtime_shutdown_does_not_deadlock() {
    use std::sync::mpsc;
    use std::time::Duration;
    // Positive control: normal registration and invalidation return normally.
    let control = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let live = service_without_pool();
    control.block_on(async {
        live.register_request("owner", "live", Uuid::new_v4(), 1, CancellationToken::new());
        live.invalidate_local_connection("owner", "live", 1);
    });
    control.shutdown_timeout(Duration::from_secs(1));
    eprintln!("CONTROL: live-runtime registration and invalidation returned");

    let (stopped_tx, stopped_rx) = mpsc::channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_stop(move || {
            let _ = stopped_tx.send(());
        })
        .build()
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let service = service_without_pool();
    runtime.spawn(async move {
        entered_tx.send(()).unwrap();
        // Simulate an already-running acquire poll reaching registration while
        // another runtime worker begins shutdown. This is a synchronous gate,
        // not a new task submitted to an already-dead runtime.
        release_rx.recv().unwrap();
        let cancel = CancellationToken::new();
        service.register_request("owner", "shutdown", Uuid::new_v4(), 1, cancel.clone());
        assert!(cancel.is_cancelled());
        let active = service.active.lock().unwrap();
        assert!(active.monitor.is_none() && active.connections.is_empty());
        done_tx.send(()).unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    runtime.shutdown_background();
    stopped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("other worker has completed shutdown");
    release_tx.send(()).unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("registration already running during shutdown must return without locking itself");
}

#[test]
fn rejected_monitor_spawn_unwinds_without_poisoning_registry() {
    let service = service_without_pool();
    let cancel = CancellationToken::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // No runtime: spawn panics and must drop its prebuilt guard safely.
        service.register_request("owner", "unwind", Uuid::new_v4(), 1, cancel.clone());
    }));
    assert!(result.is_err());
    assert!(cancel.is_cancelled());
    let active = service.active.lock().unwrap();
    assert!(active.monitor.is_none() && active.connections.is_empty());
}
