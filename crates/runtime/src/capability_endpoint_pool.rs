use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default max concurrent RPCs per registered endpoint URL.
/// Override with `ASTRA_ENDPOINT_RPC_CONCURRENCY` env var.
const DEFAULT_REGISTERED_ENDPOINT_RPC_CONCURRENCY: usize = 128;
const ENDPOINT_POOL_REAP_INTERVAL: Duration = Duration::from_secs(60);

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

struct RegisteredEndpointPool {
    semaphores: HashMap<String, Weak<Semaphore>>,
    next_reap: Instant,
    #[cfg(test)]
    reap_count: usize,
}

impl RegisteredEndpointPool {
    fn new() -> Self {
        Self {
            semaphores: HashMap::new(),
            next_reap: Instant::now() + ENDPOINT_POOL_REAP_INTERVAL,
            #[cfg(test)]
            reap_count: 0,
        }
    }

    fn semaphore(&mut self, endpoint_url: &str) -> Arc<Semaphore> {
        self.reap_idle_if_due(Instant::now());
        match self
            .semaphores
            .get(endpoint_url)
            .and_then(|entry| entry.upgrade())
        {
            Some(semaphore) => semaphore,
            None => {
                let semaphore = Arc::new(Semaphore::new(registered_endpoint_rpc_concurrency()));
                self.semaphores
                    .insert(endpoint_url.to_string(), Arc::downgrade(&semaphore));
                semaphore
            }
        }
    }

    fn reap_idle_if_due(&mut self, now: Instant) {
        if now < self.next_reap {
            return;
        }
        self.semaphores
            .retain(|_, semaphore| semaphore.strong_count() > 0);
        self.next_reap = now + ENDPOINT_POOL_REAP_INTERVAL;
        #[cfg(test)]
        {
            self.reap_count += 1;
        }
    }

    #[cfg(test)]
    fn contains(&self, endpoint_url: &str) -> bool {
        self.semaphores.contains_key(endpoint_url)
    }
}

static REGISTERED_ENDPOINT_SEMAPHORES: OnceLock<Mutex<RegisteredEndpointPool>> = OnceLock::new();

fn try_acquire_from_pool(
    pool: &Mutex<RegisteredEndpointPool>,
    endpoint_url: &str,
) -> Result<OwnedSemaphorePermit, String> {
    let semaphore = {
        let mut guard = pool.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("registered endpoint semaphore map was poisoned; recovering inner map");
            poisoned.into_inner()
        });
        guard.semaphore(endpoint_url)
    };

    semaphore
        .try_acquire_owned()
        .map_err(|_| "registered endpoint is over its concurrency limit".to_string())
}

pub(crate) fn try_acquire_endpoint_permit(
    endpoint_url: &str,
) -> Result<OwnedSemaphorePermit, String> {
    try_acquire_from_pool(
        REGISTERED_ENDPOINT_SEMAPHORES.get_or_init(|| Mutex::new(RegisteredEndpointPool::new())),
        endpoint_url,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_pool_rejects_acquire_after_limit() {
        let pool = Mutex::new(RegisteredEndpointPool::new());
        let endpoint = format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4());
        let mut permits = Vec::new();
        for _ in 0..registered_endpoint_rpc_concurrency() {
            permits.push(
                try_acquire_from_pool(&pool, &endpoint).expect("permit within endpoint limit"),
            );
        }

        let err = try_acquire_from_pool(&pool, &endpoint)
            .expect_err("endpoint limit should reject one more concurrent call");
        assert!(err.contains("over its concurrency limit"));

        drop(permits);
        let _permit =
            try_acquire_from_pool(&pool, &endpoint).expect("released permits should allow reuse");
    }

    #[test]
    fn endpoint_pool_reclaims_idle_entries_after_owner_rotation() {
        let pool = Mutex::new(RegisteredEndpointPool::new());
        let retired = format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4());
        let active = format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4());
        let retired_permit = try_acquire_from_pool(&pool, &retired).expect("retired owner permit");
        let _active_permit = try_acquire_from_pool(&pool, &active).expect("active owner permit");
        assert!(pool.lock().unwrap().contains(&retired));

        drop(retired_permit);
        pool.lock().unwrap().next_reap = Instant::now();
        let _next_permit = try_acquire_from_pool(
            &pool,
            &format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4()),
        )
        .expect("next owner permit");
        assert!(!pool.lock().unwrap().contains(&retired));
        assert!(pool.lock().unwrap().contains(&active));
    }

    #[test]
    fn many_active_endpoints_do_not_reap_on_every_acquisition() {
        let pool = Mutex::new(RegisteredEndpointPool::new());
        let mut permits = Vec::new();
        for _ in 0..5000 {
            let endpoint = format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4());
            permits.push(try_acquire_from_pool(&pool, &endpoint).expect("active permit"));
        }
        assert_eq!(permits.len(), 5000);
        let active = format!("https://capabilities.example.test/{}", uuid::Uuid::new_v4());
        let _active_permit = try_acquire_from_pool(&pool, &active).expect("existing permit");
        {
            let mut guard = pool.lock().unwrap();
            guard.next_reap = Instant::now() + Duration::from_secs(3600);
            guard.reap_count = 0;
        }

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let pool = &pool;
                let active = &active;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        let _permit =
                            try_acquire_from_pool(pool, active).expect("concurrent permit");
                    }
                });
            }
        });

        assert_eq!(pool.lock().unwrap().reap_count, 0);
        drop(permits);
    }
}
