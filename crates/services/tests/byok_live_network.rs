//! Explicit opt-in network smoke: no database, model key or inference request.
//! Set ASTRA_TEST_BYOK_MODELS_URL to a provider's public /models endpoint that
//! requires authentication, plus the Server's ASTRA_BYOK_DNS_SERVERS/proxy settings.

#[tokio::test]
#[ignore = "requires explicit public provider URL and working outbound DNS/HTTPS"]
async fn byok_live_network_reaches_provider_without_credentials() {
    let url = std::env::var("ASTRA_TEST_BYOK_MODELS_URL")
        .expect("set ASTRA_TEST_BYOK_MODELS_URL to the provider's HTTPS /models endpoint");
    let client = astra_services::byok_endpoint::endpoint_client(&url)
        .await
        .expect("real BYOK DNS resolution and public-address validation");
    let response = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .expect("pinned HTTPS connection with origin certificate validation");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "provider should reject the intentionally unauthenticated request"
    );
}
