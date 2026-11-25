use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

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

static REGISTERED_ENDPOINT_SEMAPHORES: OnceLock<
    std::sync::Mutex<HashMap<String, Weak<Semaphore>>>,
> = OnceLock::new();

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
        guard.retain(|_, semaphore| semaphore.strong_count() > 0);
        match guard.get(endpoint_url).and_then(|entry| entry.upgrade()) {
            Some(semaphore) => semaphore,
            None => {
                let semaphore = Arc::new(Semaphore::new(registered_endpoint_rpc_concurrency()));
                guard.insert(endpoint_url.to_string(), Arc::downgrade(&semaphore));
                semaphore
            }
        }
    };

    semaphore
        .try_acquire_owned()
        .map_err(|_| "registered endpoint is over its concurrency limit".to_string())
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

    #[test]
    fn endpoint_pool_reclaims_idle_entries_after_owner_rotation() {
        fn contains(endpoint: &str) -> bool {
            let semaphores = REGISTERED_ENDPOINT_SEMAPHORES
                .get_or_init(|| std::sync::Mutex::new(HashMap::new()));
            semaphores
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(endpoint)
        }

        let retired = format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4());
        let active = format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4());
        let retired_permit = try_acquire_endpoint_permit(&retired).expect("retired owner permit");
        let _active_permit = try_acquire_endpoint_permit(&active).expect("active owner permit");
        assert!(contains(&retired));

        drop(retired_permit);
        let _next_permit = try_acquire_endpoint_permit(&format!(
            "https://capabilities.example.test/{}",
            uuid::Uuid::new_v4()
        ))
        .expect("next owner permit");
        assert!(!contains(&retired), "idle owner entry was not reclaimed");
        assert!(
            contains(&active),
            "outstanding permit must retain its entry"
        );
    }
}
