//! Provider HTTP construction. One send must not hide another inference request.
//!
//! The caller owns admitted retries and per-attempt deadlines. This module owns
//! connection reuse and disables HTTP retries and redirects beneath that caller.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use astra_core::{ClassifiedError, ErrorKind};
use astra_inference_adapter::transport::ProviderTransport;

use super::client::{llm_connect_timeout, llm_total_budget};

#[derive(Default)]
struct ProviderClientCache {
    client: OnceLock<ProviderTransport>,
    initialization: Mutex<()>,
}

impl ProviderClientCache {
    fn get_or_try_init(
        &self,
        initialize: impl FnOnce() -> Result<ProviderTransport, ClassifiedError>,
    ) -> Result<&ProviderTransport, ClassifiedError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        // Initialization never spans provider I/O. Cache only a successful
        // construction so a subsequent call can recover after local repair.
        let _guard = self
            .initialization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let client = initialize()?;
        Ok(self.client.get_or_init(|| client))
    }
}

fn build_provider_client(
    builder: reqwest::ClientBuilder,
    connect_timeout: Duration,
    total_timeout: Duration,
    pool_idle: usize,
) -> Result<ProviderTransport, ClassifiedError> {
    ProviderTransport::build(builder
        .connect_timeout(connect_timeout)
        .timeout(total_timeout)
        .pool_max_idle_per_host(pool_idle))
        .map_err(|_| {
            // Builder errors can contain local configuration. Return a typed,
            // content-free diagnostic without replacing the configured client.
            ClassifiedError::new(
                ErrorKind::Unknown,
                "Provider HTTP transport initialization failed; repair local transport configuration and retry",
            )
            .with_details_json(
                serde_json::json!({
                    "code": "provider_transport_initialization_failed",
                    "stage": "transport_initialization",
                    "retry_safety": "none",
                })
                .to_string(),
            )
        })
}

pub(crate) fn global_llm_client() -> Result<&'static ProviderTransport, ClassifiedError> {
    static CACHE: ProviderClientCache = ProviderClientCache {
        client: OnceLock::new(),
        initialization: Mutex::new(()),
    };
    CACHE.get_or_try_init(|| {
        let connect = llm_connect_timeout();
        // This backstop remains above the logical call budget. Request-level
        // timeouts and the coordinator's settlement reserve stay authoritative.
        let total = llm_total_budget().saturating_add(Duration::from_secs(60));
        let pool_idle = std::env::var("ASTRA_LLM_POOL_MAX_IDLE_PER_HOST")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4);
        let builder = astra_core::net::apply_env_proxy(reqwest::Client::builder());
        let client = build_provider_client(builder, connect, total, pool_idle)?;
        tracing::info!(
            target: "astra_runtime::llm_client",
            pool_max_idle_per_host = pool_idle,
            connect_timeout_s = connect.as_secs(),
            total_timeout_s = total.as_secs(),
            "global LLM HTTP client built"
        );
        Ok(client)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_client(timeout: Duration) -> ProviderTransport {
        build_provider_client(
            reqwest::Client::builder().no_proxy(),
            Duration::from_secs(1),
            timeout,
            4,
        )
        .expect("build provider transport")
    }

    #[test]
    fn initialization_failure_is_typed_and_repairable_without_replacing_success() {
        let cache = ProviderClientCache::default();
        let sensitive_configuration =
            "invalid\nhttps://canary-user:canary-secret@canary.invalid/private-ca.pem";
        let error = cache
            .get_or_try_init(|| {
                build_provider_client(
                    reqwest::Client::builder()
                        .no_proxy()
                        .user_agent(sensitive_configuration),
                    Duration::from_secs(1),
                    Duration::from_secs(2),
                    4,
                )
            })
            .expect_err("invalid configuration must fail closed");
        assert_eq!(error.kind, ErrorKind::Unknown);
        assert!(!error.kind.is_retryable());
        let details: serde_json::Value = serde_json::from_str(
            error
                .details_json
                .as_deref()
                .expect("initialization details"),
        )
        .expect("structured initialization failure");
        assert_eq!(details["code"], "provider_transport_initialization_failed");
        assert_eq!(details["stage"], "transport_initialization");
        assert_eq!(details["retry_safety"], "none");
        for private_value in [
            "canary-user",
            "canary-secret",
            "canary.invalid",
            "private-ca.pem",
        ] {
            assert!(!error.to_string().contains(private_value));
            assert!(!format!("{error:?}").contains(private_value));
        }
        assert!(cache.client.get().is_none());
        let repaired = cache
            .get_or_try_init(|| Ok(local_client(Duration::from_secs(2))))
            .expect("repaired configuration initializes");
        let reused = cache
            .get_or_try_init(|| panic!("successful initialization must be reused"))
            .expect("reuse client");
        assert!(std::ptr::eq(repaired, reused));
    }
}
