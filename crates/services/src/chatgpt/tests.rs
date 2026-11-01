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

/// Store through a fresh epoch witness, as producers do; returns whether the
/// cache accepted the token.
fn store(
    connections: &Arc<ChatgptConnections>,
    owner: &str,
    id: &str,
    generation: i64,
    token: &str,
    version: i64,
    expires_at: i64,
) -> bool {
    let epoch = connections.cache_witness();
    connections.store_cached_token(owner, id, generation, token, version, expires_at, epoch)
}

#[tokio::test]
async fn cached_access_token_serves_repeat_reads_without_sql_or_decryption() {
    let service = service_without_pool();
    store(
        &service,
        "owner",
        "conn",
        7,
        "cached-token",
        3,
        unix_now() + 3600,
    );
    // The pool is absent: any credential-row read or decrypt would fail, so an
    // Ok result proves the cache served the token directly.
    for _ in 0..3 {
        let (token, version) = service
            .access_token("owner", "conn", 7, None)
            .await
            .expect("warm cache must not need the database");
        assert_eq!((token.as_str(), version), ("cached-token", 3));
    }
}

#[tokio::test]
async fn cached_access_token_evicts_rejected_expired_or_regenerated_entries() {
    let service = service_without_pool();
    store(
        &service,
        "owner",
        "conn",
        7,
        "cached-token",
        3,
        unix_now() + 3600,
    );
    // A caller rejecting the cached version (e.g. after a 401) must re-read
    // cross-replica state; with no pool that read fails instead of serving.
    assert!(
        service
            .access_token("owner", "conn", 7, Some(3))
            .await
            .is_err()
    );
    // The rejected version is provider-invalid for everyone: the entry is
    // evicted so later independent requests cannot re-live the 401.
    assert!(service.tokens.lock().unwrap().entries.is_empty());
    assert!(
        service
            .access_token("owner", "conn", 7, None)
            .await
            .is_err()
    );
    // Entries inside the 60-second refresh window are expired and evicted.
    store(
        &service,
        "owner",
        "conn",
        7,
        "expired-token",
        4,
        unix_now() + 30,
    );
    assert!(
        service
            .access_token("owner", "conn", 7, None)
            .await
            .is_err()
    );
    assert!(service.tokens.lock().unwrap().entries.is_empty());
    // A reconnect bumps the generation; the stale entry is evicted, not served.
    store(
        &service,
        "owner",
        "conn",
        7,
        "old-generation",
        5,
        unix_now() + 3600,
    );
    assert!(
        service
            .access_token("owner", "conn", 8, None)
            .await
            .is_err()
    );
    assert!(service.tokens.lock().unwrap().entries.is_empty());
    // A stale-generation caller must not drop the reconnect's fresher entry.
    store(
        &service,
        "owner",
        "conn",
        8,
        "new-generation",
        1,
        unix_now() + 3600,
    );
    assert!(
        service
            .access_token("owner", "conn", 7, None)
            .await
            .is_err()
    );
    let (token, version) = service
        .cached_access_token("owner", "conn", 8, None)
        .expect("newer generation survives stale-generation reads");
    assert_eq!((token.as_str(), version), ("new-generation", 1));
}

#[tokio::test]
async fn cached_access_token_never_skips_the_revocation_fence() {
    let service = service_without_pool();
    store(
        &service,
        "owner",
        "conn",
        7,
        "cached-token",
        3,
        unix_now() + 3600,
    );
    let credential = ExecutionCredential {
        service: service.clone(),
        owner: "owner".into(),
        connection_id: "conn".into(),
        generation: 7,
        label: "label".into(),
    };
    // A warm cache still requires the per-request fence read in ensure_current;
    // without a database the acquire must fail rather than issue a lease.
    assert!(credential.acquire(None).await.is_err());
}

#[tokio::test]
async fn stored_tokens_never_overwrite_newer_versions() {
    let service = service_without_pool();
    // A slow reader caching an old version must not clobber a refreshed one.
    store(
        &service,
        "owner",
        "conn",
        7,
        "fresh-token",
        4,
        unix_now() + 3600,
    );
    store(
        &service,
        "owner",
        "conn",
        7,
        "stale-token",
        3,
        unix_now() + 3600,
    );
    let (token, version) = service
        .cached_access_token("owner", "conn", 7, None)
        .expect("fresher entry survives a stale concurrent store");
    assert_eq!((token.as_str(), version), ("fresh-token", 4));
    // The same version is an idempotent re-store, not a regression.
    store(
        &service,
        "owner",
        "conn",
        7,
        "fresh-token",
        4,
        unix_now() + 3600,
    );
    assert!(
        service
            .cached_access_token("owner", "conn", 7, None)
            .is_some()
    );
    // A reconnect's newer generation wins over any older-generation version.
    store(
        &service,
        "owner",
        "conn",
        8,
        "reconnected",
        1,
        unix_now() + 3600,
    );
    let (token, version) = service
        .cached_access_token("owner", "conn", 8, None)
        .expect("reconnect replaces the older generation");
    assert_eq!((token.as_str(), version), ("reconnected", 1));
    // An older generation never overwrites the reconnect's entry.
    store(
        &service,
        "owner",
        "conn",
        7,
        "pre-reconnect",
        9,
        unix_now() + 3600,
    );
    let (token, version) = service
        .cached_access_token("owner", "conn", 8, None)
        .expect("older generation cannot clobber the reconnect entry");
    assert_eq!((token.as_str(), version), ("reconnected", 1));
}

#[tokio::test]
async fn token_cache_is_bounded_and_sweeps_expired_first() {
    let service = service_without_pool();
    let now = unix_now();
    // Expired entries are reclaimed before any live one is evicted.
    store(&service, "owner", "expired", 1, "dead", 1, now + 10);
    for index in 0..MAX_CACHED_TOKENS {
        store(
            &service,
            "owner",
            &format!("live-{index}"),
            1,
            "tok",
            1,
            now + 3600 + index as i64,
        );
    }
    {
        let cache = service.tokens.lock().unwrap();
        assert_eq!(cache.entries.len(), MAX_CACHED_TOKENS);
        assert!(
            !cache
                .entries
                .contains_key(&("owner".to_owned(), "expired".to_owned()))
        );
    }
    // Beyond capacity the soonest-expiring live entry leaves first.
    store(&service, "owner", "newest", 1, "tok", 1, now + 7200);
    let cache = service.tokens.lock().unwrap();
    assert_eq!(cache.entries.len(), MAX_CACHED_TOKENS);
    assert!(
        !cache
            .entries
            .contains_key(&("owner".to_owned(), "live-0".to_owned()))
    );
    assert!(
        cache
            .entries
            .contains_key(&("owner".to_owned(), "newest".to_owned()))
    );
}

#[tokio::test]
async fn invalidate_local_connection_evicts_tokens_and_cancels_fenced_leases() {
    let service = service_without_pool();
    store(
        &service,
        "owner",
        "conn",
        7,
        "cached-token",
        3,
        unix_now() + 3600,
    );
    let fenced = CancellationToken::new();
    service.register_request("owner", "conn", Uuid::new_v4(), 7, fenced.clone());
    // A request from a newer generation is outside this invalidation fence.
    let newer = CancellationToken::new();
    service.register_request("owner", "conn", Uuid::new_v4(), 8, newer.clone());
    service.invalidate_local_connection("owner", "conn", 7);
    assert!(fenced.is_cancelled());
    assert!(!newer.is_cancelled());
    assert!(
        service.tokens.lock().unwrap().entries.is_empty(),
        "cleared credentials must not leave plaintext tokens cached"
    );
    let active = service.active.lock().unwrap();
    let connection = &active[&("owner".to_owned(), "conn".to_owned())];
    assert_eq!(connection.requests.len(), 1);
    assert_eq!(connection.requests[0].1, 8);
}

#[tokio::test]
async fn rejected_floor_blocks_recache_after_eviction() {
    let service = service_without_pool();
    store(
        &service,
        "owner",
        "conn",
        7,
        "cached-token",
        3,
        unix_now() + 3600,
    );
    // A 401 for version 3 records the floor and evicts the entry; without a
    // database the rejected acquire fails instead of serving.
    assert!(
        service
            .access_token("owner", "conn", 7, Some(3))
            .await
            .is_err()
    );
    assert!(
        service
            .cached_token_version_for_test("owner", "conn")
            .is_none()
    );
    // Eviction must not forget the rejection: a row read paused before the
    // 401 can neither re-cache nor serve version 3 nor anything older.
    let epoch = service.cache_witness();
    assert!(!service.store_cached_token(
        "owner",
        "conn",
        7,
        "cached-token",
        3,
        unix_now() + 3600,
        epoch
    ));
    assert!(!service.store_cached_token(
        "owner",
        "conn",
        7,
        "older-token",
        2,
        unix_now() + 3600,
        epoch
    ));
    assert!(
        service
            .cached_access_token("owner", "conn", 7, None)
            .is_none()
    );
    // The rotation answering the rejection is above the floor and caches.
    assert!(service.store_cached_token(
        "owner",
        "conn",
        7,
        "fresh-token",
        4,
        unix_now() + 3600,
        epoch
    ));
    let (token, version) = service
        .cached_access_token("owner", "conn", 7, None)
        .expect("the version above the floor is servable");
    assert_eq!((token.as_str(), version), ("fresh-token", 4));
}

#[tokio::test]
async fn invalidation_severs_paused_store_witnesses() {
    let service = service_without_pool();
    // A reader snapshots the epoch, then is paused while disconnect or
    // re-auth commits and invalidates.
    let stale_epoch = service.cache_witness();
    service.invalidate_local_connection("owner", "conn", 7);
    // Resuming, it must not re-insert the pre-invalidation plaintext.
    assert!(!service.store_cached_token(
        "owner",
        "conn",
        7,
        "paused-read",
        3,
        unix_now() + 3600,
        stale_epoch
    ));
    assert!(
        service
            .cached_access_token("owner", "conn", 7, None)
            .is_none()
    );
    // A witness taken after the invalidation stores normally again.
    assert!(store(
        &service,
        "owner",
        "conn",
        7,
        "post-invalidation",
        3,
        unix_now() + 3600
    ));
    assert_eq!(
        service.cached_token_version_for_test("owner", "conn"),
        Some(3)
    );
    // A reconnect severs witnesses from the replaced registration as well.
    let pre_reconnect = service.cache_witness();
    service.bump_cache_epoch();
    assert!(!service.store_cached_token(
        "owner",
        "conn",
        7,
        "pre-reconnect",
        3,
        unix_now() + 3600,
        pre_reconnect
    ));
}

#[tokio::test]
async fn guard_state_is_bounded() {
    let service = service_without_pool();
    for index in 0..(MAX_CACHED_TOKENS + 8) {
        service.note_rejected_version("owner", &format!("conn-{index}"), 1, 1);
    }
    assert!(service.tokens.lock().unwrap().guards.len() <= MAX_CACHED_TOKENS);
}

#[tokio::test]
async fn duplicate_rejection_recording_does_not_sever_fresh_witnesses() {
    let service = service_without_pool();
    // Two callers racing the same 401 both record it; only the first record
    // advances the floor and severs witnesses. A caller that witnessed the
    // clock between the two records must still cache the rotated token:
    // re-bumping on a duplicate record would refuse its store, send it back
    // to re-read and re-record the same rejection, and sever the other
    // caller in turn, so both could starve behind a single connection pool
    // despite holding a usable new token.
    service.note_rejected_version("owner", "conn", 7, 3);
    let between = service.cache_witness();
    service.note_rejected_version("owner", "conn", 7, 3);
    service.note_rejected_version("owner", "conn", 7, 2);
    assert!(service.store_cached_token(
        "owner",
        "conn",
        7,
        "rotated",
        4,
        unix_now() + 3600,
        between
    ));
    assert_eq!(
        service.cached_token_version_for_test("owner", "conn"),
        Some(4)
    );
}

#[tokio::test]
async fn newer_rejection_still_severs_witnesses() {
    let service = service_without_pool();
    service.note_rejected_version("owner", "conn", 7, 3);
    let stale = service.cache_witness();
    // A genuinely newer 401 is fresh evidence: the floor advances and
    // witnesses taken in between are severed.
    service.note_rejected_version("owner", "conn", 7, 4);
    assert!(!service.store_cached_token(
        "owner",
        "conn",
        7,
        "paused-read",
        5,
        unix_now() + 3600,
        stale
    ));
    // A reconnect restarts the version sequence under a new generation: its
    // first rejection advances the floor again, and a duplicate record of it
    // is a no-op.
    service.note_rejected_version("owner", "conn", 8, 1);
    let between = service.cache_witness();
    service.note_rejected_version("owner", "conn", 8, 1);
    assert!(service.store_cached_token(
        "owner",
        "conn",
        8,
        "gen-eight",
        2,
        unix_now() + 3600,
        between
    ));
}

#[tokio::test]
async fn capacity_eviction_cannot_rebirth_paused_store_witnesses() {
    let service = service_without_pool();
    // Regression: a per-connection epoch living in the capacity-evictable
    // guard map resets when its guard is evicted, so enough unrelated guard
    // pressure lets a paused reader's witness alias the recreated state
    // (epoch ABA) and re-insert plaintext behind a disconnect. The global
    // monotonic clock never resets, so no eviction revives a stale witness.
    let stale = service.cache_witness();
    service.invalidate_local_connection("owner", "conn", 7);
    for index in 0..(MAX_CACHED_TOKENS * 2) {
        service.note_rejected_version("owner", &format!("other-{index}"), 1, 1);
    }
    {
        let cache = service.tokens.lock().unwrap();
        assert!(cache.guards.len() <= MAX_CACHED_TOKENS);
        assert!(cache.entries.len() <= MAX_CACHED_TOKENS);
    }
    assert!(!service.store_cached_token(
        "owner",
        "conn",
        7,
        "paused-read",
        3,
        unix_now() + 3600,
        stale
    ));
    assert!(
        service
            .cached_access_token("owner", "conn", 7, None)
            .is_none()
    );
}

#[tokio::test]
async fn rejection_severs_paused_witnesses_even_after_floor_eviction() {
    let service = service_without_pool();
    // A row read paused beside a 401 must not re-cache the rejected version
    // even if guard capacity pressure later drops the floor record itself:
    // recording the rejection bumped the invalidation clock, which eviction
    // cannot reset. The floor check alone had a hole here.
    let stale = service.cache_witness();
    service.note_rejected_version("owner", "conn", 7, 3);
    for index in 0..(MAX_CACHED_TOKENS * 2) {
        service.note_rejected_version("owner", &format!("other-{index}"), 1, 1);
    }
    assert!(!service.store_cached_token(
        "owner",
        "conn",
        7,
        "rejected",
        3,
        unix_now() + 3600,
        stale
    ));
    assert!(
        service
            .cached_access_token("owner", "conn", 7, None)
            .is_none()
    );
    // A post-rejection witness for a rotated version still stores normally.
    assert!(store(
        &service,
        "owner",
        "conn",
        7,
        "rotated",
        4,
        unix_now() + 3600
    ));
    assert_eq!(
        service.cached_token_version_for_test("owner", "conn"),
        Some(4)
    );
}

#[tokio::test]
async fn settled_flight_rejected_between_check_and_store_is_re_adjudicated() {
    let service = service_without_pool();
    let expires = unix_now() + 3600;
    // A refresh flight witnesses the invalidation clock before its exchange.
    let flight_epoch = service.cache_witness();
    // The flight settles version 4; the waiter's rejection check passes.
    // Before its store runs, a concurrent caller draws a 401 for exactly
    // that settled version and records the floor (which also severs the
    // flight witness). The store refuses, and the waiter must not serve the
    // rejected plaintext it is still holding.
    service.note_rejected_version("owner", "conn", 7, 4);
    match service.adjudicate_settled_flight("owner", "conn", 7, "token-4", 4, expires, flight_epoch)
    {
        SettledFlight::Retry { rejected } => assert!(
            rejected,
            "a settled version refused at the recorded floor must rotate past it"
        ),
        SettledFlight::Serve(token, version) => panic!(
            "a concurrently rejected settled version must not be served: ({token}, {version})"
        ),
    }
    // The refused token never entered the cache and stays unservable.
    assert!(
        service
            .cached_access_token("owner", "conn", 7, None)
            .is_none()
    );
    assert!(
        service
            .cached_token_version_for_test("owner", "conn")
            .is_none()
    );
    // The rotation answering the rejection settles version 5 above the
    // floor with a post-rejection witness and is served and cached.
    let rotated = service.cache_witness();
    match service.adjudicate_settled_flight("owner", "conn", 7, "token-5", 5, expires, rotated) {
        SettledFlight::Serve(token, version) => {
            assert_eq!((token.as_str(), version), ("token-5", 5));
        }
        SettledFlight::Retry { .. } => {
            panic!("a settled version above the floor with a fresh witness must be served")
        }
    }
    assert_eq!(
        service.cached_token_version_for_test("owner", "conn"),
        Some(5)
    );
}

#[tokio::test]
async fn settled_flight_overtaken_by_newer_entry_retries_without_rejection() {
    let service = service_without_pool();
    let expires = unix_now() + 3600;
    let flight_epoch = service.cache_witness();
    // While the flight was exchanging, another caller's rotation cached
    // version 5. The flight's version 4 store loses the monotonic race; the
    // waiter must retry and re-evaluate against the newer entry instead of
    // serving the older token it holds, and version 4 is not rejected.
    assert!(store(&service, "owner", "conn", 7, "token-5", 5, expires));
    match service.adjudicate_settled_flight("owner", "conn", 7, "token-4", 4, expires, flight_epoch)
    {
        SettledFlight::Retry { rejected } => assert!(
            !rejected,
            "a store lost to a newer entry is not rejection evidence"
        ),
        SettledFlight::Serve(token, version) => panic!(
            "a settled version overtaken by a newer entry must not be served: ({token}, {version})"
        ),
    }
    // The retried caller is then served the newer entry from the cache.
    let (token, version) = service
        .cached_access_token("owner", "conn", 7, None)
        .expect("the newer entry remains servable");
    assert_eq!((token.as_str(), version), ("token-5", 5));
}

#[tokio::test]
async fn settled_flight_overtaken_by_invalidation_retries_and_recovers() {
    let service = service_without_pool();
    let expires = unix_now() + 3600;
    let flight_epoch = service.cache_witness();
    // A disconnect or re-auth commits while the flight is exchanging: the
    // invalidation severs the flight witness, so the store refuses and the
    // waiter must retry rather than serve pre-invalidation plaintext. The
    // settled version itself was never rejected.
    service.invalidate_local_connection("owner", "conn", 7);
    match service.adjudicate_settled_flight("owner", "conn", 7, "token-4", 4, expires, flight_epoch)
    {
        SettledFlight::Retry { rejected } => assert!(
            !rejected,
            "an invalidation-severed witness is not rejection evidence"
        ),
        SettledFlight::Serve(token, version) => panic!(
            "a settled version behind an invalidation must not be served: ({token}, {version})"
        ),
    }
    assert!(
        service
            .cached_access_token("owner", "conn", 7, None)
            .is_none()
    );
    // The retried caller re-reads post-invalidation credentials and stores
    // them under a fresh witness: the retry path recovers instead of
    // latching on the severed one.
    let fresh = service.cache_witness();
    match service.adjudicate_settled_flight("owner", "conn", 7, "token-5", 5, expires, fresh) {
        SettledFlight::Serve(token, version) => {
            assert_eq!((token.as_str(), version), ("token-5", 5));
        }
        SettledFlight::Retry { .. } => {
            panic!("a settled version stored under a fresh witness must be served")
        }
    }
}
