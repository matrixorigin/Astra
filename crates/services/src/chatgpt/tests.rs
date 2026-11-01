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
