//! Capability registry — registers providers and resolves tool requests.
//!
//! The registry is the central routing mechanism: it matches incoming
//! `ToolRequest`s to the best available `CapabilityProvider` based on
//! capability, storage access, isolation level, and priority.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::{debug, warn};

use crate::provider::traits::{CapabilityProvider, ProviderError, ToolRequest};
use crate::provider::types::{ToolCapability, ToolCategory};
use astra_runtime_env::IsolationIntent;

/// Number of consecutive failures before a provider is considered unhealthy.
const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// Cooldown after which a tripped provider gets one probe request (half-open).
/// Without this, a provider tripped by transient failures is skipped forever
/// because it never gets a chance to `record_success` and reset.
const CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// RegisteredProvider — internal wrapper
// ---------------------------------------------------------------------------

/// A provider registered in the registry, together with the cached metadata.
struct RegisteredProvider {
    /// The provider implementation.
    provider: Arc<dyn CapabilityProvider>,
    /// Capabilities this provider can fulfill (cached at registration time).
    capabilities: Vec<ToolCapability>,
    /// Whether this provider can access workspace storage.
    storage_accessible: bool,
    /// Circuit breaker: consecutive execution failures.
    consecutive_failures: AtomicU32,
    /// Wall-clock of the most recent failure. Once `CIRCUIT_BREAKER_COOLDOWN`
    /// elapses, the provider enters half-open and receives one probe request.
    last_failure_time: Mutex<Option<Instant>>,
    /// Half-open probe throttle: only one concurrent probe request is allowed
    /// per cooldown window. Wrapped in `Arc` so a `ProbeGuard` returned from
    /// `resolve()` can release it on Drop — guaranteeing the slot is freed even
    /// if the caller's future is dropped (Tokio cancellation) before reaching
    /// `record_success`/`record_failure`.
    probe_in_flight: Arc<AtomicBool>,
}

/// RAII guard that releases the half-open probe slot on Drop.
///
/// Returned from `resolve()` alongside the provider so the slot is freed even
/// if the caller's future is dropped (Tokio cancellation) or panics before
/// reaching `record_success`/`record_failure` — closing the permanent
/// lock-out that occurred when only `record_*` released the slot.
pub struct ProbeGuard {
    slot: Arc<AtomicBool>,
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.slot.store(false, Ordering::Release);
    }
}

/// A provider resolved by the registry, optionally carrying a probe guard.
pub struct ResolvedProvider {
    pub provider: Arc<dyn CapabilityProvider>,
    /// Present when this provider was in half-open state and we claimed the
    /// single allowed probe slot. Dropping this guard (explicitly or via
    /// future cancellation) releases the slot.
    pub probe_guard: Option<ProbeGuard>,
}

impl std::ops::Deref for ResolvedProvider {
    type Target = dyn CapabilityProvider;
    fn deref(&self) -> &Self::Target {
        &*self.provider
    }
}

// ---------------------------------------------------------------------------
// CapabilityRegistry
// ---------------------------------------------------------------------------

/// Thread-safe registry of capability providers.
///
/// # Usage
///
/// ```ignore
/// let registry = CapabilityRegistry::new();
/// registry.register("builtin-shell", Arc::new(my_provider)).await?;
/// let provider = registry.resolve(&tool_request).await?;
/// ```
pub struct CapabilityRegistry {
    providers: Arc<DashMap<String, RegisteredProvider>>,
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            providers: Arc::new(DashMap::new()),
        }
    }

    /// Register a provider under a given name.
    ///
    /// Capabilities are cached at registration time for fast lookups.
    /// Returns an error if a provider with the same name already exists.
    pub async fn register(
        &self,
        name: impl Into<String>,
        provider: Arc<dyn CapabilityProvider>,
    ) -> Result<(), ProviderError> {
        let name: String = name.into();

        // Cache capabilities and storage access at registration time.
        let capabilities = provider.capabilities().await;
        let storage_accessible = provider.storage_accessible().await;

        let registered = RegisteredProvider {
            provider,
            capabilities,
            storage_accessible,
            consecutive_failures: AtomicU32::new(0),
            last_failure_time: Mutex::new(None),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
        };

        // Insert or fail if name collision — use entry() for atomic
        // check-and-insert to prevent TOCTOU races between concurrent
        // register calls for the same provider name.
        match self.providers.entry(name.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                return Err(ProviderError::Internal(format!(
                    "provider '{name}' is already registered"
                )));
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(registered);
            }
        }
        debug!(provider = %name, "registered capability provider");
        Ok(())
    }

    /// Resolve a tool request to the best available provider.
    ///
    /// Resolution steps:
    /// 1. Filter by capability (exact named match first, then category).
    /// 2. Filter by storage access (when request specifies storage).
    /// 3. Filter by health (circuit breaker: skip providers with >= threshold failures).
    /// 4. Filter by isolation level (provider must satisfy request).
    /// 5. Sort by priority (lower = preferred), then return first.
    pub async fn resolve(&self, request: &ToolRequest) -> Result<ResolvedProvider, ProviderError> {
        // Phase 1 (sync): filter by capability, storage access, and circuit breaker.
        // We clone the Arc to escape the dashmap lock guard lifetime. The third
        // tuple element flags whether this candidate is a half-open probe (tripped
        // but cooled down); the probe slot is NOT claimed here — claiming it for a
        // candidate that later loses the priority sort would leak, because only
        // the winner ever reaches record_success/record_failure to release it.
        let candidates: Vec<(String, Arc<dyn CapabilityProvider>, bool)> = self
            .providers
            .iter()
            .filter_map(|entry| {
                let registered = entry.value();

                // Step 1: Capability matching.
                if !capability_matches(&request.capability, &registered.capabilities) {
                    return None;
                }

                // Step 2: Storage-aware filtering.
                if request.storage.is_some() && !registered.storage_accessible {
                    return None;
                }

                // Step 3: Circuit breaker — skip providers that have tripped,
                // unless the cooldown has elapsed (half-open). Tripped-and-cooled
                // providers stay as candidates but are flagged `needs_probe`; the
                // single probe slot is claimed in Phase 3 for the chosen provider
                // only, so a slot is never claimed for a provider we don't run.
                let failures = registered
                    .consecutive_failures
                    .load(std::sync::atomic::Ordering::Relaxed);
                let needs_probe = if failures >= CIRCUIT_BREAKER_THRESHOLD {
                    let cooled_down = registered
                        .last_failure_time
                        .lock()
                        .map(|t| {
                            t.as_ref()
                                .map(|i| i.elapsed() >= CIRCUIT_BREAKER_COOLDOWN)
                                .unwrap_or(true)
                        })
                        .unwrap_or(true);
                    if !cooled_down {
                        debug!(
                            provider = %entry.key(),
                            failures,
                            "circuit breaker open — skipping unhealthy provider"
                        );
                        return None;
                    }
                    true
                } else {
                    false
                };

                Some((
                    entry.key().clone(),
                    Arc::clone(&registered.provider),
                    needs_probe,
                ))
            })
            .collect();

        if candidates.is_empty() {
            // Distinguish: no capable provider vs all capable providers tripped.
            let any_capable = self.providers.iter().any(|entry| {
                capability_matches(&request.capability, &entry.value().capabilities)
                    && (request.storage.is_none() || entry.value().storage_accessible)
            });
            if any_capable {
                return Err(ProviderError::Unhealthy(format!(
                    "all providers for {:?} have tripped circuit breaker (>= {} failures)",
                    request.capability, CIRCUIT_BREAKER_THRESHOLD
                )));
            }
            return Err(ProviderError::NotCapable {
                capability: request.capability.clone(),
            });
        }

        // Phase 2 (async): check isolation and live provider health, then
        // collect priorities. Circuit-breaker state captures recent execution
        // failures; health_check() catches providers that are registered but
        // not currently usable, such as missing transports or expired leases.
        // Carry `needs_probe` forward so Phase 3 can claim the slot.
        let isolation_required = request.isolation_required;

        let mut eligible: Vec<(String, u8, bool)> = Vec::with_capacity(candidates.len());
        let mut any_isolation_match = false;
        let mut health_failures = Vec::new();
        for (name, provider, needs_probe) in &candidates {
            let provider_isolation = provider.isolation_level();
            if provider_isolation.satisfies(isolation_required) {
                any_isolation_match = true;
                match provider.health_check().await {
                    Ok(()) => {
                        let priority = provider.priority();
                        eligible.push((name.clone(), priority, *needs_probe));
                    }
                    Err(err) => {
                        debug!(
                            provider = %name,
                            error = ?err,
                            "provider health check failed during resolution"
                        );
                        health_failures.push((name.clone(), err));
                    }
                }
            }
        }

        if !any_isolation_match {
            return Err(ProviderError::Isolation(format!(
                "no provider satisfies isolation level {isolation_required:?}"
            )));
        }

        if eligible.is_empty() {
            return Err(ProviderError::Unhealthy(format!(
                "all providers for {:?} failed health checks: {}",
                request.capability,
                summarize_health_failures(&health_failures)
            )));
        }

        // Phase 3: sort by priority (lower = preferred) and pick best
        // available. Iterate in priority order so that if the top provider
        // was concurrently unregistered we gracefully fall back to the next
        // one instead of panicking. This is also where the half-open probe
        // slot is claimed: only for the provider we actually return. If the
        // chosen provider's slot is already in flight, fall through to the
        // next candidate rather than stampeding the recovering backend.
        eligible.sort_by_key(|(_, p, _)| *p);

        for (name, _, needs_probe) in &eligible {
            if let Some(entry) = self.providers.get(name) {
                let probe_guard = if *needs_probe {
                    // CAS-claim the single probe slot for this provider. If a
                    // concurrent resolver already claimed it, fall through to
                    // the next candidate instead of stampeding the backend.
                    if entry
                        .probe_in_flight
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_err()
                    {
                        debug!(
                            provider = %name,
                            "circuit breaker half-open — probe already in flight, trying next provider"
                        );
                        continue;
                    }
                    debug!(
                        provider = %name,
                        "circuit breaker half-open — probing after cooldown"
                    );
                    Some(ProbeGuard {
                        slot: Arc::clone(&entry.probe_in_flight),
                    })
                } else {
                    None
                };
                return Ok(ResolvedProvider {
                    provider: Arc::clone(&entry.provider),
                    probe_guard,
                });
            }
        }
        // All previously-eligible providers were concurrently unregistered,
        // or all half-open candidates already had a probe in flight.
        Err(ProviderError::Unhealthy(format!(
            "no usable provider for {:?}: all eligible providers tripped with probe in flight",
            request.capability
        )))
    }

    /// Record a successful execution for a provider, resetting its circuit breaker.
    ///
    /// Does NOT touch `probe_in_flight` — the `ProbeGuard` returned from
    /// `resolve()` owns the slot lifetime and releases it on Drop.
    pub fn record_success(&self, name: &str) {
        if let Some(entry) = self.providers.get(name) {
            entry
                .consecutive_failures
                .store(0, std::sync::atomic::Ordering::Relaxed);
            if let Ok(mut t) = entry.last_failure_time.lock() {
                *t = None;
            }
        }
    }

    /// Record a failed execution for a provider, incrementing its circuit breaker.
    ///
    /// Does NOT touch `probe_in_flight` — the `ProbeGuard` returned from
    /// `resolve()` owns the slot lifetime and releases it on Drop.
    pub fn record_failure(&self, name: &str) {
        if let Some(entry) = self.providers.get(name) {
            let prev = entry
                .consecutive_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if prev + 1 >= CIRCUIT_BREAKER_THRESHOLD {
                if let Ok(mut t) = entry.last_failure_time.lock() {
                    *t = Some(Instant::now());
                }
                warn!(
                    provider = %name,
                    failures = prev + 1,
                    "circuit breaker tripped for provider"
                );
            }
        }
    }

    /// List all providers capable of handling a given capability.
    pub fn list_capable(&self, capability: &ToolCapability) -> Vec<Arc<dyn CapabilityProvider>> {
        self.providers
            .iter()
            .filter(|entry| capability_matches(capability, &entry.capabilities))
            .map(|entry| Arc::clone(&entry.provider))
            .collect()
    }

    /// Run a health check on all registered providers.
    pub async fn health_check_all(&self) -> Vec<(String, Result<(), ProviderError>)> {
        let mut results = Vec::with_capacity(self.providers.len());
        for entry in self.providers.iter() {
            let name = entry.key().clone();
            let result = entry.provider.health_check().await;
            results.push((name, result));
        }
        results
    }
}

#[cfg(test)]
impl CapabilityRegistry {
    /// Test-only helper to remove a provider by name.
    fn unregister(&self, name: &str) -> bool {
        self.providers.remove(name).is_some()
    }
}

// ---------------------------------------------------------------------------
// Capability matching logic
// ---------------------------------------------------------------------------

/// Returns true if `requested` matches any capability in `offered`.
fn capability_matches(requested: &ToolCapability, offered: &[ToolCapability]) -> bool {
    offered.iter().any(|cap| cap_matches(requested, cap))
}

/// Single-pair capability matcher.
fn cap_matches(requested: &ToolCapability, offered: &ToolCapability) -> bool {
    match (requested, offered) {
        // Exact named match.
        (ToolCapability::Named(a), ToolCapability::Named(b)) => a == b,
        // Same category.
        (ToolCapability::Category(a), ToolCapability::Category(b)) => a == b,
        // Named request against category offer: the category provider can
        // handle only known tools that belong to that category.
        // Category request against named offer: a provider offering only
        // one named tool cannot satisfy a full category request.
        (ToolCapability::Named(name), ToolCapability::Category(category)) => {
            ToolCategory::for_tool_name(name)
                .map(|tool_category| tool_category == *category)
                .unwrap_or(false)
        }
        (ToolCapability::Category(_), ToolCapability::Named(_)) => false,
    }
}

fn summarize_health_failures(failures: &[(String, ProviderError)]) -> String {
    if failures.is_empty() {
        return "none".into();
    }
    failures
        .iter()
        .map(|(name, err)| format!("{name}: {err:?}"))
        .collect::<Vec<_>>()
        .join("; ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::types::{ProviderKind, ToolCategory};
    use crate::storage::{MountType, StorageAccess};

    // A minimal stub provider for testing.
    struct StubProvider {
        kind: ProviderKind,
        capabilities: Vec<ToolCapability>,
        priority: u8,
        isolation: IsolationIntent,
        storage_accessible: bool,
    }

    #[async_trait::async_trait]
    impl CapabilityProvider for StubProvider {
        fn kind(&self) -> ProviderKind {
            self.kind
        }

        async fn capabilities(&self) -> Vec<ToolCapability> {
            self.capabilities.clone()
        }

        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }

        async fn execute(&self, _request: ToolRequest) -> crate::provider::traits::ToolResult {
            crate::provider::traits::ToolResult::Success {
                data: serde_json::Value::Null,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }

        fn priority(&self) -> u8 {
            self.priority
        }

        fn isolation_level(&self) -> IsolationIntent {
            self.isolation
        }

        async fn storage_accessible(&self) -> bool {
            self.storage_accessible
        }
    }

    fn stub_provider(
        capabilities: Vec<ToolCapability>,
        priority: u8,
        storage_accessible: bool,
    ) -> StubProvider {
        StubProvider {
            kind: ProviderKind::ServerBuiltin,
            capabilities,
            priority,
            isolation: IsolationIntent::None,
            storage_accessible,
        }
    }

    // ── Registration ────────────────────────────────────────────────────

    #[tokio::test]
    async fn register_and_resolve_single_provider() {
        let reg = CapabilityRegistry::new();
        let provider = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            10,
            false,
        ));
        reg.register("p1", provider.clone()).await.unwrap();

        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-1".into(),
            parameters: serde_json::json!({"cmd": "ls"}),
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let resolved = reg.resolve(&request).await.unwrap();
        assert_eq!(resolved.provider.priority(), 10);
    }

    #[tokio::test]
    async fn resolve_no_capable_provider_returns_error() {
        let reg = CapabilityRegistry::new();
        let provider = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            10,
            false,
        ));
        reg.register("p1", provider).await.unwrap();

        let request = ToolRequest {
            capability: ToolCapability::Named("nonexistent".into()),
            tool_name: "nonexistent".into(),
            tool_call_id: "call-1".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let result = reg.resolve(&request).await;
        assert!(matches!(result, Err(ProviderError::NotCapable { .. })));
    }

    // ── Priority routing ────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_picks_lowest_priority() {
        let reg = CapabilityRegistry::new();

        let low = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            1,
            false,
        ));
        let high = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            100,
            false,
        ));
        reg.register("low-prio", low.clone()).await.unwrap();
        reg.register("high-prio", high.clone()).await.unwrap();

        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-2".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let resolved = reg.resolve(&request).await.unwrap();
        assert_eq!(resolved.provider.priority(), 1);
    }

    // ── Storage-aware filtering ─────────────────────────────────────────

    #[tokio::test]
    async fn storage_aware_filtering() {
        let reg = CapabilityRegistry::new();

        let with_storage = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            10,
            true,
        ));
        let without_storage = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            5,
            false,
        ));
        reg.register("with-storage", with_storage.clone())
            .await
            .unwrap();
        reg.register("no-storage", without_storage.clone())
            .await
            .unwrap();

        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-3".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: Some(StorageAccess {
                mount_path: "/workspace".into(),
                mount_type: MountType::Bind,
                read_only: false,
            }),
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        // Should pick "with-storage" even though it has higher priority,
        // because "no-storage" is filtered out.
        let resolved = reg.resolve(&request).await.unwrap();
        assert!(resolved.provider.storage_accessible().await);
    }

    // ── Isolation filtering ─────────────────────────────────────────────

    #[tokio::test]
    async fn isolation_filtering() {
        let reg = CapabilityRegistry::new();

        let container_provider = Arc::new(StubProvider {
            kind: ProviderKind::SandboxRuntime,
            capabilities: vec![ToolCapability::Named("bash".into())],
            priority: 10,
            isolation: IsolationIntent::Container,
            storage_accessible: false,
        });
        let process_provider = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            5,
            false,
        ));
        reg.register("container", container_provider.clone())
            .await
            .unwrap();
        reg.register("process", process_provider.clone())
            .await
            .unwrap();

        // Request requiring Container isolation — process provider should be filtered.
        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-4".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::Container,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let resolved = reg.resolve(&request).await.unwrap();
        assert_eq!(
            resolved.provider.isolation_level(),
            IsolationIntent::Container
        );
    }

    // ── Category matching ───────────────────────────────────────────────

    #[tokio::test]
    async fn category_matching() {
        let reg = CapabilityRegistry::new();

        let shell_provider = Arc::new(stub_provider(
            vec![ToolCapability::Category(ToolCategory::Shell)],
            5,
            false,
        ));
        reg.register("shell-category", shell_provider.clone())
            .await
            .unwrap();

        // Request a specific Shell tool name — category provider should match.
        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-5".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let resolved = reg.resolve(&request).await;
        assert!(resolved.is_ok());
    }

    #[tokio::test]
    async fn named_tool_does_not_match_wrong_category_provider() {
        let reg = CapabilityRegistry::new();

        reg.register(
            "shell-category",
            Arc::new(stub_provider(
                vec![ToolCapability::Category(ToolCategory::Shell)],
                5,
                false,
            )),
        )
        .await
        .unwrap();

        let request = ToolRequest {
            capability: ToolCapability::Named("memory".into()),
            tool_name: "memory".into(),
            tool_call_id: "call-wrong-category".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let resolved = reg.resolve(&request).await;
        assert!(
            matches!(resolved, Err(ProviderError::NotCapable { .. })),
            "StateManagement tool must not match Shell provider"
        );
    }

    #[tokio::test]
    async fn unknown_named_tool_does_not_match_category_provider() {
        let reg = CapabilityRegistry::new();

        reg.register(
            "filesystem-category",
            Arc::new(stub_provider(
                vec![ToolCapability::Category(ToolCategory::FileSystem)],
                5,
                true,
            )),
        )
        .await
        .unwrap();

        let request = ToolRequest {
            capability: ToolCapability::Named("unknown_tool".into()),
            tool_name: "unknown_tool".into(),
            tool_call_id: "call-unknown-tool".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let resolved = reg.resolve(&request).await;
        assert!(
            matches!(resolved, Err(ProviderError::NotCapable { .. })),
            "unknown named tool must not match category provider"
        );
    }

    // ── list_capable ────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_capable_returns_matching_providers() {
        let reg = CapabilityRegistry::new();

        reg.register(
            "p1",
            Arc::new(stub_provider(
                vec![ToolCapability::Named("bash".into())],
                1,
                false,
            )),
        )
        .await
        .unwrap();
        reg.register(
            "p2",
            Arc::new(stub_provider(
                vec![ToolCapability::Named("bash".into())],
                2,
                false,
            )),
        )
        .await
        .unwrap();

        let capable = reg.list_capable(&ToolCapability::Named("bash".into()));
        assert_eq!(capable.len(), 2);
    }

    // ── health_check_all ────────────────────────────────────────────────

    #[tokio::test]
    async fn health_check_all_returns_all() {
        let reg = CapabilityRegistry::new();

        reg.register(
            "p1",
            Arc::new(stub_provider(
                vec![ToolCapability::Named("bash".into())],
                1,
                false,
            )),
        )
        .await
        .unwrap();

        let results = reg.health_check_all().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok());
    }

    // ── Duplicate registration ──────────────────────────────────────────

    #[tokio::test]
    async fn duplicate_registration_returns_error() {
        let reg = CapabilityRegistry::new();

        let provider = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            1,
            false,
        ));
        reg.register("p1", provider.clone()).await.unwrap();

        let result = reg.register("p1", provider.clone()).await;
        assert!(result.is_err());
    }

    // ── TOCTOU resilience ───────────────────────────────────────────────

    /// If the top-priority provider is concurrently unregistered between
    /// isolation-level probing and provider lookup, resolve must gracefully
    /// fall back to the next eligible provider instead of panicking.
    #[tokio::test]
    async fn toctou_unregister_during_resolve_falls_back() {
        let reg = CapabilityRegistry::new();

        let best = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            1,
            false,
        ));
        let fallback = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            2,
            false,
        ));
        reg.register("best", best.clone()).await.unwrap();
        reg.register("fallback", fallback.clone()).await.unwrap();

        // Remove the best provider before resolution — simulates concurrent
        // unregistration between the sync capability-gathering and the
        // async isolation-level phase (or between sort and get).
        assert!(reg.unregister("best"));

        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-toctou".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let resolved = reg.resolve(&request).await.unwrap();
        // Should fall back to the remaining provider, not panic.
        assert_eq!(resolved.provider.priority(), 2);
    }

    /// If ALL eligible providers are unregistered concurrently, resolve
    /// returns NotCapable instead of panicking.
    #[tokio::test]
    async fn toctou_all_providers_removed_returns_not_capable() {
        let reg = CapabilityRegistry::new();

        let p1 = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            1,
            false,
        ));
        let p2 = Arc::new(stub_provider(
            vec![ToolCapability::Named("bash".into())],
            2,
            false,
        ));
        reg.register("p1", p1.clone()).await.unwrap();
        reg.register("p2", p2.clone()).await.unwrap();

        // Remove all providers for the capability.
        assert!(reg.unregister("p1"));
        assert!(reg.unregister("p2"));

        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-toctou-all".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };

        let result = reg.resolve(&request).await;
        assert!(
            matches!(result, Err(ProviderError::NotCapable { .. })),
            "should return NotCapable, not panic, when all providers are gone"
        );
    }
}
