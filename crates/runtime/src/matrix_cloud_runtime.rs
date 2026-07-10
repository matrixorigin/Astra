//! Matrix-backed cloud plumbing in one place: [`SharedPool`], journal ingestion, and
//! [`SyncOrchestrator`] (learning, events, templates, preferences). Used by `astra-server`
//! [`AppState`] and by the CLI [`ReplState`] as a single `Arc` attachment.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinSet;

use astra_core::{MatrixOneSettings, SharedPool};
use astra_services::{
    CloudTransport, SyncOrchestrator, TaskLeaseHoldCache, TaskRecord,
    event_ingestion::{self, IngestionConfig, IngestionEvent},
    session_journal::JournalEvent,
    state_sync::MatrixOneSyncService,
};

/// Max time to wait for the ingestion worker to finish during shutdown.
const INGESTION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Trait for tracking fire-and-forget persist futures so they are drained on shutdown.
///
/// `InProcessChatTurnBridge` uses this to hand off its SSE-generator persist tasks to
/// a shutdown-aware tracker rather than raw `tokio::spawn`.  The production impl is
/// [`MatrixCloudRuntime`]; tests inject a lightweight stub.
///
/// Object-safe: uses `Pin<Box<dyn Future>>` instead of a generic parameter.
pub trait BridgePersistTracker: Send + Sync {
    fn track_persist_task(&self, task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>);
}

/// Environment-driven MatrixOne settings.
///
/// Requires `MATRIXONE_PASSWORD` to be set — fails closed rather than
/// substituting a hardcoded development password.
pub fn matrix_settings_from_env() -> Result<MatrixOneSettings, String> {
    MatrixOneSettings::from_env_strict()
}

/// Production resolver that reads the `"selector"`-tagged cheap model
/// from the `infra_llm_models` registry. Used to feed
/// [`crate::session_memory::MemoryExtractionService`] without pulling
/// the full MatrixCloudRuntime into every caller.
pub struct PoolSelectorResolver {
    pool: SharedPool,
    encryptor: Arc<astra_services::FernetTokenEncryptor>,
}

impl PoolSelectorResolver {
    pub fn new(pool: SharedPool, encryptor: Arc<astra_services::FernetTokenEncryptor>) -> Self {
        Self { pool, encryptor }
    }
}

impl std::fmt::Debug for PoolSelectorResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolSelectorResolver").finish()
    }
}

#[async_trait::async_trait]
impl crate::session_memory::SelectorParamsResolver for PoolSelectorResolver {
    async fn resolve(&self) -> Option<crate::memory_hooks::relevance::LlmConnParams> {
        self.resolve_candidates().await.into_iter().next()
    }

    async fn resolve_candidates(&self) -> Vec<crate::memory_hooks::relevance::LlmConnParams> {
        let settings = self.pool.settings();
        let pool = self.pool.get();
        let resolved =
            astra_services::models::resolve_memory_models(settings, &self.encryptor, Some(pool))
                .await
                .unwrap_or_default();
        resolved
            .into_iter()
            .map(|model| crate::memory_hooks::relevance::LlmConnParams {
                base_url: model.base_url,
                api_key: model.api_key,
                model_name: model.model_name,
                wire_model_name: model.wire_model_name,
                provider: model.provider,
                request_body_overrides: model.request_body_overrides,
                thinking_capability: model.thinking_capability,
            })
            .collect()
    }
}

/// Pool + ingestion + unified sync orchestrator. Safe to share behind `Arc`.
pub struct MatrixCloudRuntime {
    shared_pool: SharedPool,
    ingestion: Mutex<Option<astra_services::event_ingestion::IngestionSender>>,
    /// Join handle for the background ingestion worker — awaited on graceful shutdown
    /// to ensure all buffered events are flushed to cloud before process exit.
    ingestion_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Notify-based shutdown signal — works even when cloned senders are still alive.
    ingestion_shutdown: astra_services::event_ingestion::IngestionShutdownHandle,
    /// Session/cloud sync tasks spawned from the CLI (checkpoint, session-state,
    /// context-trace pushes). Awaited on graceful shutdown so short sessions do not
    /// silently lose the final cloud sidecars.
    session_sync_tasks: Mutex<JoinSet<()>>,
    /// Live ingestion stats (events_received, events_flushed, errors).
    ingestion_stats: Arc<std::sync::Mutex<astra_services::event_ingestion::IngestionStats>>,
    /// Normalized ingestion config used by this runtime instance.
    ingestion_config: astra_services::event_ingestion::IngestionConfig,
    sync_orchestrator: TokioMutex<SyncOrchestrator>,
    /// Phase 3: shared with HTTP lease handlers (process-local export filter).
    pub lease_hold_cache: Arc<TaskLeaseHoldCache>,
    /// Phase 3: local task mirror.
    pub task_mirror: Arc<Mutex<BTreeMap<String, TaskRecord>>>,
    /// Phase 3: dirty task IDs pending sync.
    pub task_dirty: Arc<Mutex<HashSet<String>>>,
    edge_agent_id: Arc<str>,
    sync_service: Arc<MatrixOneSyncService>,
    audit_flusher_shutdown: tokio_util::sync::CancellationToken,
    audit_flusher_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    encryptor: Option<Arc<astra_services::FernetTokenEncryptor>>,
    /// Lazy slot for the session-memory extraction coordinator. The
    /// service depends on the encryptor (for selector resolution) so
    /// we can't build it in [`Self::attach`]; it's populated by
    /// [`Self::with_encryptor`] when both pool and encryptor are
    /// available, and exposed via
    /// [`Self::clone_memory_extraction_service`].
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    /// User ID this runtime was attached for — needed when constructing
    /// the memory extraction service inside [`Self::with_encryptor`].
    user_id: Arc<str>,
}

impl MatrixCloudRuntime {
    /// Wire ingestion worker to an existing [`SharedPool`]. The
    /// [`SyncOrchestrator`] is constructed bare (no adapters) since the
    /// per-domain sync adapters (learning, events, templates, preferences,
    /// tasks) were removed along with the self-evolution subsystem.
    pub fn attach(
        shared_pool: SharedPool,
        _profile: &str,
        user_id: &str,
        lease_hold_cache: Arc<TaskLeaseHoldCache>,
    ) -> Self {
        let edge_agent_id: Arc<str> = std::env::var("ASTRA_EDGE_AGENT_ID")
            .unwrap_or_else(|_| "astra-server".into())
            .into();
        let task_mirror = Arc::new(Mutex::new(BTreeMap::new()));
        let task_dirty = Arc::new(Mutex::new(HashSet::new()));

        let pool = shared_pool.get().clone();
        let ingestion_config = IngestionConfig::default();
        let (sender, ingestion_shutdown, ingestion_stats, ingestion_jh) =
            event_ingestion::EventIngestionWorker::spawn(pool.clone(), ingestion_config.clone());
        let audit_flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
        let sync_svc = Arc::new(MatrixOneSyncService::new(
            pool,
            audit_flusher.writer.clone(),
        ));
        let transport: Arc<dyn CloudTransport> = Arc::new(astra_services::NoopTransport);
        let orch = SyncOrchestrator::new(transport, user_id.to_string());

        Self {
            shared_pool,
            ingestion: Mutex::new(Some(sender)),
            ingestion_handle: Mutex::new(Some(ingestion_jh)),
            ingestion_shutdown,
            session_sync_tasks: Mutex::new(JoinSet::new()),
            ingestion_stats,
            ingestion_config,
            sync_orchestrator: TokioMutex::new(orch),
            lease_hold_cache,
            task_mirror,
            task_dirty,
            edge_agent_id,
            sync_service: sync_svc,
            audit_flusher_shutdown: audit_flusher.shutdown,
            audit_flusher_handle: Mutex::new(Some(audit_flusher.join_handle)),
            encryptor: None,
            memory_extraction_service: None,
            user_id: Arc::from(user_id),
        }
    }

    /// Attach a token encryptor for decrypting model API keys from the
    /// DB. Call this after `attach()` when the encryptor is available.
    ///
    /// Also spins up the [`crate::session_memory::MemoryExtractionService`]
    /// here, because it needs all three of: encryptor (for selector
    /// resolve), ingestion sender (for events), and a [`MemoriaPort`]
    /// (the sole persistence target for L1 session memory). If
    /// [`HttpMemoriaPort::from_env`] returns `None` (no Memoria
    /// endpoint configured / offline), the service is NOT built —
    /// extraction is opt-in on connectivity, not silent fallback.
    pub fn with_encryptor(mut self, enc: Arc<astra_services::FernetTokenEncryptor>) -> Self {
        self.encryptor = Some(Arc::clone(&enc));
        let ingestion = self.ingestion.lock().ok().and_then(|g| g.as_ref().cloned());
        let memoria = crate::turn::cloud::memoria_compact::HttpMemoriaPort::from_env();
        if let (Some(ingestion), Some(memoria)) = (ingestion, memoria) {
            let resolver: Arc<dyn crate::session_memory::SelectorParamsResolver> =
                Arc::new(PoolSelectorResolver {
                    pool: self.shared_pool.clone(),
                    encryptor: Arc::clone(&enc),
                });
            let broker = Arc::new(crate::session_memory::BackgroundActivityBroker::new());
            let memoria_client: Arc<dyn crate::turn::cloud::memoria_compact::MemoriaPort> =
                Arc::new(memoria);
            let svc = Arc::new(crate::session_memory::MemoryExtractionService::new(
                resolver,
                memoria_client,
                ingestion,
                Arc::clone(&self.user_id),
                broker,
            ));
            self.memory_extraction_service = Some(svc);
        }
        self
    }

    /// Clone the memory-extraction coordinator for consumers (server
    /// lifecycle service, CLI repl state). `None` if `with_encryptor`
    /// hasn't been called, or ingestion was already shut down.
    pub fn clone_memory_extraction_service(
        &self,
    ) -> Option<Arc<crate::session_memory::MemoryExtractionService>> {
        self.memory_extraction_service.clone()
    }

    /// Resolve the cheapest memory-tagged model from the registry.
    /// Returns `None` if no encryptor is configured or resolution fails.
    pub async fn resolve_memory_model(
        &self,
    ) -> Option<crate::memory_hooks::relevance::LlmConnParams> {
        let enc = self.encryptor.as_ref()?;
        let settings = self.shared_pool.settings();
        let pool = self.shared_pool.get();
        let resolved = astra_services::models::resolve_memory_model(settings, enc, Some(pool))
            .await
            .ok()?;
        Some(crate::memory_hooks::relevance::LlmConnParams {
            base_url: resolved.base_url,
            api_key: resolved.api_key,
            model_name: resolved.model_name,
            wire_model_name: resolved.wire_model_name,
            provider: resolved.provider,
            request_body_overrides: resolved.request_body_overrides,
            thinking_capability: resolved.thinking_capability,
        })
    }

    pub fn edge_agent_id(&self) -> &str {
        &self.edge_agent_id
    }
    pub fn shared_pool(&self) -> &SharedPool {
        &self.shared_pool
    }

    /// Shared sync service for push operations (checkpoints, session state, context traces).
    ///
    /// Callers that spawn background tasks holding an `Arc` clone **must** use
    /// [`spawn_session_sync_task`] so the runtime can drain them before shutting
    /// down the audit flusher. Tasks spawned outside that mechanism may lose
    /// audit entries on shutdown.
    pub fn sync_service(&self) -> &Arc<MatrixOneSyncService> {
        &self.sync_service
    }

    /// Snapshot of ingestion stats (events received/flushed/errors + overflow).
    pub fn ingestion_stats(&self) -> Option<astra_services::event_ingestion::IngestionStats> {
        self.ingestion_stats.lock().ok().map(|s| s.clone())
    }

    /// Normalized ingestion configuration used by this runtime instance.
    pub fn ingestion_config(&self) -> &astra_services::event_ingestion::IngestionConfig {
        &self.ingestion_config
    }

    /// Number of immediate ingestion overflows, including bounded deferred sends
    /// and closed-channel drops.
    pub fn ingestion_overflow_count(&self) -> u64 {
        self.ingestion
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.overflow_count()))
            .unwrap_or(0)
    }

    /// Number of ingestion events dropped before the worker accepted them.
    pub fn ingestion_dropped_before_acceptance_count(&self) -> u64 {
        self.ingestion
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.dropped_before_acceptance_count()))
            .unwrap_or(0)
    }

    pub fn ingestion_dropped_critical_before_acceptance_count(&self) -> u64 {
        self.ingestion
            .lock()
            .ok()
            .and_then(|g| {
                g.as_ref()
                    .map(|s| s.dropped_critical_before_acceptance_count())
            })
            .unwrap_or(0)
    }

    pub fn ingestion_dropped_telemetry_before_acceptance_count(&self) -> u64 {
        self.ingestion
            .lock()
            .ok()
            .and_then(|g| {
                g.as_ref()
                    .map(|s| s.dropped_telemetry_before_acceptance_count())
            })
            .unwrap_or(0)
    }

    /// Clone the ingestion sender for use in other subsystems (e.g., durable task lifecycle).
    /// Returns `None` if ingestion is shut down or lock is poisoned.
    pub fn clone_ingestion_sender(
        &self,
    ) -> Option<astra_services::event_ingestion::IngestionSender> {
        self.ingestion.lock().ok()?.as_ref().cloned()
    }

    /// Spawn a session/cloud sync sidecar task and track it for graceful shutdown.
    pub fn spawn_session_sync_task<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if let Ok(mut tasks) = self.session_sync_tasks.lock() {
            while let Some(result) = tasks.try_join_next() {
                if let Err(error) = result {
                    astra_core::agent_warn!(
                        "session_sync",
                        "session sync task join failed: {error}"
                    );
                }
            }
            tasks.spawn(task);
        } else {
            tokio::spawn(task);
        }
    }

    /// Expand a journal event and enqueue for async DB flush (no-op if ingestion shut down).
    pub fn enqueue_journal_events(&self, user_id: &str, event: &JournalEvent) {
        let Ok(guard) = self.ingestion.lock() else {
            return;
        };
        let Some(sender) = guard.as_ref() else {
            return;
        };
        let events = match IngestionEvent::expand_journal_event_with_redact(
            event,
            user_id,
            self.ingestion_config.redact_content,
        ) {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::matrix_cloud_runtime",
                    error = %error,
                    "invalid journal event for cloud ingestion"
                );
                return;
            }
        };
        for ev in events {
            sender.enqueue(ev);
        }
    }

    /// Enqueue a content-addressed config-version push. The worker
    /// dual-writes to `agent_events` (standard trail) and
    /// `config_versions` (tenant-scoped blob + TOML body). No-op if
    /// ingestion has been shut down. Idempotent on the server side
    /// via INSERT IGNORE on (user_id, version_id).
    pub fn enqueue_config_version_push(
        &self,
        row: &astra_services::config_version_cloud::ConfigVersionPayload,
    ) {
        let Ok(guard) = self.ingestion.lock() else {
            return;
        };
        let Some(sender) = guard.as_ref() else {
            return;
        };
        let event = match IngestionEvent::for_config_version(row) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::matrix_cloud_runtime",
                    version_id = %row.version_id,
                    error = %error,
                    "invalid config version push event"
                );
                return;
            }
        };
        sender.enqueue(event);
    }

    /// Flush and stop the ingestion worker, then **wait** for the background
    /// worker plus tracked session/cloud sync tasks to drain before process exit.
    pub async fn shutdown_ingestion_and_wait(&self) {
        // Signal the worker to exit via Notify — works even when cloned senders
        // are still alive (the channel-close approach fails in that case).
        self.ingestion_shutdown.signal();
        // Also drop our sender to release the channel reference.
        if let Ok(mut g) = self.ingestion.lock() {
            g.take();
        }
        // Await the worker join handle with a timeout.
        let handle = self.ingestion_handle.lock().ok().and_then(|mut g| g.take());
        if let Some(jh) = handle {
            match tokio::time::timeout(INGESTION_SHUTDOWN_TIMEOUT, jh).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    astra_core::agent_warn!("ingestion", "worker join failed: {e}");
                }
                Err(_) => {
                    astra_core::agent_warn!(
                        "ingestion",
                        "worker flush timed out after {INGESTION_SHUTDOWN_TIMEOUT:?}, some events may be lost"
                    );
                }
            }
        }

        // Drain session sync tasks FIRST — they hold audit writer clones that
        // must be dropped before we can close the audit channel.
        let mut session_sync_tasks = self
            .session_sync_tasks
            .lock()
            .ok()
            .map(|mut tasks| std::mem::take(&mut *tasks))
            .unwrap_or_default();
        if !session_sync_tasks.is_empty() {
            match tokio::time::timeout(INGESTION_SHUTDOWN_TIMEOUT, async {
                while let Some(result) = session_sync_tasks.join_next().await {
                    if let Err(error) = result {
                        astra_core::agent_warn!(
                            "session_sync",
                            "session sync task join failed: {error}"
                        );
                    }
                }
            })
            .await
            {
                Ok(()) => {}
                Err(_) => {
                    astra_core::agent_warn!(
                        "session_sync",
                        "session sync drain timed out after {INGESTION_SHUTDOWN_TIMEOUT:?}, some sync sidecars may be lost"
                    );
                }
            }
        }

        // Signal the audit flusher to drain and exit. CancellationToken is
        // level-triggered — stays cancelled once cancelled, so the flusher sees
        // it regardless of poll timing. Works even though SyncAuditWriter clones
        // inside Arc<MatrixOneSyncService> keep the channel open.
        self.audit_flusher_shutdown.cancel();
        let audit_handle = self
            .audit_flusher_handle
            .lock()
            .ok()
            .and_then(|mut g| g.take());
        if let Some(jh) = audit_handle {
            match tokio::time::timeout(INGESTION_SHUTDOWN_TIMEOUT, jh).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    astra_core::agent_warn!("audit_flusher", "audit flusher join failed: {e}");
                }
                Err(_) => {
                    astra_core::agent_warn!(
                        "audit_flusher",
                        "audit flusher drain timed out after {INGESTION_SHUTDOWN_TIMEOUT:?}"
                    );
                }
            }
        }
    }

    pub async fn sync_orchestrator_lock(&self) -> tokio::sync::MutexGuard<'_, SyncOrchestrator> {
        self.sync_orchestrator.lock().await
    }
}

impl BridgePersistTracker for MatrixCloudRuntime {
    fn track_persist_task(&self, task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        self.spawn_session_sync_task(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_settings_from_env_non_empty() {
        // SAFETY: tests run sequentially in this module; setting env temporarily.
        unsafe { std::env::set_var("MATRIXONE_PASSWORD", "test-pw") };
        let s = matrix_settings_from_env().expect("password set");
        assert!(!s.host.is_empty());
        assert!(s.port > 0);
        assert!(!s.database.is_empty());
        unsafe { std::env::remove_var("MATRIXONE_PASSWORD") };
    }

    /// HIGH #4: BridgePersistTracker functional test — future runs via a minimal test impl.
    #[tokio::test]
    async fn bridge_persist_tracker_future_runs_and_drains() {
        use std::pin::Pin;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::oneshot;

        struct SpawningTracker;
        impl BridgePersistTracker for SpawningTracker {
            fn track_persist_task(&self, task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
                tokio::spawn(task);
            }
        }

        let task_ran = Arc::new(AtomicBool::new(false));
        let task_ran2 = task_ran.clone();
        let (tx, rx) = oneshot::channel::<()>();

        let tracker: Arc<dyn BridgePersistTracker> = Arc::new(SpawningTracker);
        tracker.track_persist_task(Box::pin(async move {
            task_ran2.store(true, Ordering::SeqCst);
            let _ = tx.send(());
        }));

        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), rx).await;
        assert!(
            task_ran.load(Ordering::SeqCst),
            "tracked future must execute"
        );
    }
}
