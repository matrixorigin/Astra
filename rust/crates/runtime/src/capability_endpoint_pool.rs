use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default max concurrent RPCs per registered endpoint URL.
/// Override with `ASTRA_ENDPOINT_RPC_CONCURRENCY` env var.
const DEFAULT_REGISTERED_ENDPOINT_RPC_CONCURRENCY: usize = 128;

fn registered_endpoint_rpc_concurrency() -> usize {
    static CELL: OnceLock<usize> = OnceLock::new();
    *CELL.get_or_init(|| {
        std::env::var("ASTRA_ENDPOINT_RPC_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_REGISTERED_ENDPOINT_RPC_CONCURRENCY)
    })
}

#[cfg(test)]
pub(crate) const REGISTERED_ENDPOINT_RPC_CONCURRENCY_FOR_TESTS: usize =
    DEFAULT_REGISTERED_ENDPOINT_RPC_CONCURRENCY;

static REGISTERED_ENDPOINT_SEMAPHORES: OnceLock<std::sync::Mutex<HashMap<String, Arc<Semaphore>>>> =
    OnceLock::new();

pub(crate) fn try_acquire_endpoint_permit(
    endpoint_url: &str,
) -> Result<OwnedSemaphorePermit, String> {
    let semaphores =
        REGISTERED_ENDPOINT_SEMAPHORES.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let semaphore = {
        let mut guard = semaphores.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("registered endpoint semaphore map was poisoned; recovering inner map");
            poisoned.into_inner()
        });
        guard
            .entry(endpoint_url.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(registered_endpoint_rpc_concurrency())))
            .clone()
    };

    semaphore
        .try_acquire_owned()
        .map_err(|_| format!("registered endpoint '{endpoint_url}' is over its concurrency limit"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_pool_rejects_acquire_after_limit() {
        let endpoint = format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4());
        let mut permits = Vec::new();
        for _ in 0..registered_endpoint_rpc_concurrency() {
            permits.push(
                try_acquire_endpoint_permit(&endpoint).expect("permit within endpoint limit"),
            );
        }

        let err = try_acquire_endpoint_permit(&endpoint)
            .expect_err("endpoint limit should reject one more concurrent call");
        assert!(err.contains("over its concurrency limit"));

        drop(permits);
        let _permit =
            try_acquire_endpoint_permit(&endpoint).expect("released permits should allow reuse");
    }
}
