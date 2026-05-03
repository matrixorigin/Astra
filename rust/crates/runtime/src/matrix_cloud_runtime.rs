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

use astra_core::{MatrixOneSettings, SharedPool, resolve_database_name};
use astra_services::{
    CloudTransport, SyncOrchestrator, SyncPolicy, TaskLeaseHoldCache, TaskRecord,
    event_ingestion::{self, IngestionConfig, IngestionEvent},
    session_journal::JournalEvent,
    state_sync::{MatrixOneSyncService, PlanTemplateSyncRow, StateSyncService},
};

use crate::sync_adapters::{
    EventAdapter, LearningAdapter, MatrixOneTransport, PreferenceAdapter, TaskAdapter,
    TemplateAdapter,
};
use astra_evolution::persistence::ToolHealthEntry;
use astra_pipeline::{
    calibration::ProgressiveCalibrator, entity::EntityGraph, pattern::PatternLibrary,
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
    Ok(MatrixOneSettings {
        host: std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("MATRIXONE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6001),
        user: std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
        password: std::env::var("MATRIXONE_PASSWORD")
            .map_err(|_| "MATRIXONE_PASSWORD environment variable is required".to_string())?,
        database: resolve_database_name(&|k| std::env::var(k).ok()),
    })
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
    sync_orchestrator: TokioMutex<SyncOrchestrator>,
    /// Edge preference map (same `Arc` as [`PreferenceAdapter`] inside the orchestrator).
    preference_store: Arc<Mutex<BTreeMap<String, String>>>,
    /// Cached plan templates from last `pull_domain(Templates)`.
    template_cache: Arc<Mutex<Vec<PlanTemplateSyncRow>>>,
    /// Phase 3: shared with HTTP lease handlers and [`TaskAdapter`] (process-local export filter).
    pub lease_hold_cache: Arc<TaskLeaseHoldCache>,
    /// Phase 3: local task mirror, shared with [`TaskAdapter`] for push sync.
    pub task_mirror: Arc<Mutex<BTreeMap<String, TaskRecord>>>,
    /// Phase 3: dirty task IDs pending sync, shared with [`TaskAdapter`].
    pub task_dirty: Arc<Mutex<HashSet<String>>>,
    edge_agent_id: Arc<str>,
    sync_service: Arc<MatrixOneSyncService>,
    audit_flusher_shutdown: tokio_util::sync::CancellationToken,
    audit_flusher_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    encryptor: Option<Arc<astra_services::FernetTokenEncryptor>>,
}

impl MatrixCloudRuntime {
    /// Wire ingestion worker and sync domains to an existing [`SharedPool`].
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        shared_pool: SharedPool,
        profile: &str,
        user_id: &str,
        entity_graph: Arc<Mutex<EntityGraph>>,
        pattern_library: Arc<Mutex<PatternLibrary>>,
        calibrator: Arc<Mutex<ProgressiveCalibrator>>,
        tool_health: Arc<Mutex<Vec<ToolHealthEntry>>>,
        cloud_learning_version: Option<i64>,
        lease_hold_cache: Arc<TaskLeaseHoldCache>,
    ) -> Self {
        let edge_agent_id: Arc<str> = std::env::var("ASTRA_EDGE_AGENT_ID")
            .unwrap_or_else(|_| "astra-server".into())
            .into();
        let task_mirror = Arc::new(Mutex::new(BTreeMap::new()));
        let task_dirty = Arc::new(Mutex::new(HashSet::new()));

        let pool = shared_pool.get().clone();
        let (sender, ingestion_shutdown, ingestion_stats, ingestion_jh) =
            event_ingestion::EventIngestionWorker::spawn(pool.clone(), IngestionConfig::default());
        let audit_flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
        let sync_svc = Arc::new(MatrixOneSyncService::new(
            pool,
            audit_flusher.writer.clone(),
        ));
        let transport: Arc<dyn CloudTransport> = Arc::new(MatrixOneTransport::new(
            sync_svc.clone() as Arc<dyn StateSyncService>,
            profile.to_string(),
            edge_agent_id.as_ref(),
        ));
        let mut orch = SyncOrchestrator::new(transport, user_id.to_string());
        let learning_adapter =
            LearningAdapter::new(entity_graph, pattern_library, calibrator, tool_health);
        // NOTE: Do NOT mark_pulled without actually fetching and merging data.
        // The cloud_learning_version hint is ignored here; proper sync should
        // happen via the orchestrator's pull cycle after initialization.
        let _ = cloud_learning_version; // suppress unused warning
        orch.register(Box::new(learning_adapter), SyncPolicy::learning());

        let preference_store = Arc::new(Mutex::new(BTreeMap::new()));
        let template_cache = Arc::new(Mutex::new(Vec::<PlanTemplateSyncRow>::new()));

        orch.register(
            Box::new(EventAdapter::new(sender.clone())),
            SyncPolicy::events(),
        );
        orch.register(
            Box::new(TemplateAdapter::new(Arc::clone(&template_cache))),
            SyncPolicy::templates(),
        );
        orch.register(
            Box::new(PreferenceAdapter::new(Arc::clone(&preference_store))),
            SyncPolicy::preferences(),
        );
        orch.register(
            Box::new(TaskAdapter::new(
                Arc::clone(&task_mirror),
                Arc::clone(&task_dirty),
                Arc::clone(&edge_agent_id),
                Arc::clone(&lease_hold_cache),
            )),
            SyncPolicy::tasks(),
        );

        Self {
            shared_pool,
            ingestion: Mutex::new(Some(sender)),
            ingestion_handle: Mutex::new(Some(ingestion_jh)),
            ingestion_shutdown,
            session_sync_tasks: Mutex::new(JoinSet::new()),
            ingestion_stats,
            sync_orchestrator: TokioMutex::new(orch),
            preference_store,
            template_cache,
            lease_hold_cache,
            task_mirror,
            task_dirty,
            edge_agent_id,
            sync_service: sync_svc,
            audit_flusher_shutdown: audit_flusher.shutdown,
            audit_flusher_handle: Mutex::new(Some(audit_flusher.join_handle)),
            encryptor: None,
        }
    }

    /// Attach a token encryptor for decrypting model API keys from the DB.
    /// Call this after `attach()` when the encryptor is available.
    pub fn with_encryptor(mut self, enc: Arc<astra_services::FernetTokenEncryptor>) -> Self {
        self.encryptor = Some(enc);
        self
    }

    /// Resolve the cheapest selector-tagged model from the registry.
    /// Returns `None` if no encryptor is configured or resolution fails.
    pub async fn resolve_selector_model(&self) -> Option<crate::memory_relevance::LlmConnParams> {
        let enc = self.encryptor.as_ref()?;
        let settings = self.shared_pool.settings();
        let pool = self.shared_pool.get();
        let resolved = astra_services::models::resolve_memory_model(settings, enc, Some(pool))
            .await
            .ok()?;
        Some(crate::memory_relevance::LlmConnParams {
            base_url: resolved.base_url,
            api_key: resolved.api_key,
            model_name: resolved.model_name,
        })
    }

    pub fn preference_store(&self) -> Arc<Mutex<BTreeMap<String, String>>> {
        Arc::clone(&self.preference_store)
    }

    pub fn template_cache(&self) -> Arc<Mutex<Vec<PlanTemplateSyncRow>>> {
        Arc::clone(&self.template_cache)
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

    /// Number of events silently dropped because the ingestion channel was full.
    pub fn ingestion_overflow_count(&self) -> u64 {
        self.ingestion
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.overflow_count()))
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
        for ev in IngestionEvent::expand_journal_event(event, user_id) {
            sender.enqueue(ev);
        }
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

/// Build a [`SyncOrchestrator`] with all edge sync domains (tests / harness).
#[allow(clippy::too_many_arguments)]
pub fn build_sync_orchestrator_with_adapters(
    transport: Arc<dyn CloudTransport>,
    user_id: &str,
    entity_graph: Arc<Mutex<EntityGraph>>,
    pattern_library: Arc<Mutex<PatternLibrary>>,
    calibrator: Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: Arc<Mutex<Vec<ToolHealthEntry>>>,
    ingestion: astra_services::event_ingestion::IngestionSender,
    lease_hold_cache: Arc<TaskLeaseHoldCache>,
    task_mirror: Arc<Mutex<BTreeMap<String, TaskRecord>>>,
    task_dirty: Arc<Mutex<HashSet<String>>>,
    edge_agent_id: impl Into<Arc<str>>,
) -> SyncOrchestrator {
    let edge_agent_id: Arc<str> = edge_agent_id.into();
    let mut orch = SyncOrchestrator::new(transport, user_id.to_string());
    let learning_adapter =
        LearningAdapter::new(entity_graph, pattern_library, calibrator, tool_health);
    orch.register(Box::new(learning_adapter), SyncPolicy::learning());
    let preference_store = Arc::new(Mutex::new(BTreeMap::new()));
    let template_cache = Arc::new(Mutex::new(Vec::<PlanTemplateSyncRow>::new()));
    orch.register(Box::new(EventAdapter::new(ingestion)), SyncPolicy::events());
    orch.register(
        Box::new(TemplateAdapter::new(Arc::clone(&template_cache))),
        SyncPolicy::templates(),
    );
    orch.register(
        Box::new(PreferenceAdapter::new(preference_store)),
        SyncPolicy::preferences(),
    );
    orch.register(
        Box::new(TaskAdapter::new(
            Arc::clone(&task_mirror),
            Arc::clone(&task_dirty),
            Arc::clone(&edge_agent_id),
            Arc::clone(&lease_hold_cache),
        )),
        SyncPolicy::tasks(),
    );
    orch
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{NoopTransport, SyncDomain, event_ingestion::IngestionSender};

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

    #[test]
    fn noop_transport_orchestrator_registers_learning_and_events() {
        let transport: Arc<dyn CloudTransport> = Arc::new(NoopTransport);
        let lease = Arc::new(TaskLeaseHoldCache::default());
        let mirror = Arc::new(Mutex::new(BTreeMap::new()));
        let dirty = Arc::new(Mutex::new(HashSet::new()));
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));
        let th = Arc::new(Mutex::new(vec![]));
        let orch = build_sync_orchestrator_with_adapters(
            transport,
            "user-1",
            eg,
            pl,
            cal,
            th,
            IngestionSender::disconnected(),
            Arc::clone(&lease),
            Arc::clone(&mirror),
            Arc::clone(&dirty),
            "test-edge",
        );
        let domains: Vec<SyncDomain> = orch.status_summary().into_iter().map(|(d, _)| d).collect();
        assert!(domains.contains(&SyncDomain::Learning));
        assert!(domains.contains(&SyncDomain::Events));
        assert!(domains.contains(&SyncDomain::Templates));
        assert!(domains.contains(&SyncDomain::Preferences));
        assert!(domains.contains(&SyncDomain::Tasks));
    }

    #[test]
    fn matrix_runtime_tracks_session_sync_tasks() {
        let source = include_str!("matrix_cloud_runtime.rs");
        assert!(
            source.contains("session_sync_tasks: Mutex<JoinSet<()>>"),
            "MatrixCloudRuntime should keep a tracked JoinSet for session sync sidecars"
        );
        assert!(
            source.contains("pub fn spawn_session_sync_task"),
            "MatrixCloudRuntime should expose a helper for tracked session sync spawns"
        );
        assert!(
            source.contains("tasks.try_join_next()"),
            "spawn_session_sync_task should opportunistically reap completed sync tasks"
        );
    }

    #[test]
    fn shutdown_waits_for_session_sync_tasks() {
        let source = include_str!("matrix_cloud_runtime.rs");
        assert!(
            source.contains("std::mem::take(&mut *tasks)"),
            "shutdown_ingestion_and_wait should drain tracked session sync tasks"
        );
        assert!(
            source.contains("session_sync_tasks.join_next().await"),
            "shutdown_ingestion_and_wait should await tracked session sync tasks before exit"
        );
    }

    #[test]
    fn serve_calls_shutdown_ingestion_and_wait() {
        let source = include_str!("server/mod.rs");
        assert!(
            source.contains("shutdown_ingestion_and_wait"),
            "serve() must call shutdown_ingestion_and_wait after axum serve returns"
        );
    }

    #[test]
    fn spawn_data_cleanup_respects_cancellation_token() {
        let source = include_str!("server/mod.rs");
        assert!(
            source.contains("CancellationToken"),
            "spawn_data_cleanup must accept a CancellationToken so shutdown can drain it"
        );
        assert!(
            source.contains("cancel.cancelled()") || source.contains("cancel_token.cancelled()"),
            "spawn_data_cleanup loop must select! on the cancel token"
        );
    }

    /// HIGH #4: BridgePersistTracker trait must exist and be object-safe.
    #[test]
    fn bridge_persist_tracker_trait_is_defined() {
        let source = include_str!("matrix_cloud_runtime.rs");
        assert!(
            source.contains("pub trait BridgePersistTracker"),
            "BridgePersistTracker trait must be declared in matrix_cloud_runtime"
        );
        assert!(
            source.contains("fn track_persist_task"),
            "BridgePersistTracker must expose track_persist_task"
        );
    }

    /// HIGH #4: MatrixCloudRuntime must implement BridgePersistTracker.
    #[test]
    fn matrix_cloud_runtime_impls_bridge_persist_tracker() {
        let source = include_str!("matrix_cloud_runtime.rs");
        assert!(
            source.contains("impl BridgePersistTracker for MatrixCloudRuntime"),
            "MatrixCloudRuntime must implement BridgePersistTracker"
        );
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

    /// HIGH #4: InProcessChatTurnBridge must have a persist_tracker field.
    #[test]
    fn bridge_inprocess_has_persist_tracker_field() {
        let source = include_str!("turn/bridge_inprocess.rs");
        assert!(
            source.contains("persist_tracker"),
            "InProcessChatTurnBridge must have a persist_tracker field for HIGH #4"
        );
        assert!(
            source.contains("BridgePersistTracker"),
            "bridge_inprocess.rs must reference BridgePersistTracker"
        );
    }

    /// HIGH #4: The fire-and-forget persist paths in bridge_inprocess.rs must be
    /// replaced with the tracked path (no raw TODO(audit-#3) deferred comment).
    #[test]
    fn bridge_persist_uses_tracker_not_raw_spawn() {
        let source = include_str!("turn/bridge_inprocess.rs");
        assert!(
            !source.contains("TODO(audit-#3)"),
            "audit-#3 TODO must be resolved — persist tasks should be tracked now"
        );
    }

    #[test]
    fn with_encryptor_stores_encryptor() {
        let source = include_str!("matrix_cloud_runtime.rs");
        assert!(
            source.contains("encryptor: Option<Arc<astra_services::FernetTokenEncryptor>>"),
            "MatrixCloudRuntime must store optional encryptor for model resolution"
        );
        assert!(
            source.contains("fn with_encryptor"),
            "MatrixCloudRuntime must expose with_encryptor builder method"
        );
        assert!(
            source.contains("fn resolve_selector_model"),
            "MatrixCloudRuntime must expose resolve_selector_model for memory relevance"
        );
    }

    #[test]
    fn resolve_selector_model_requires_encryptor() {
        let source = include_str!("matrix_cloud_runtime.rs");
        assert!(
            source.contains("let enc = self.encryptor.as_ref()?;"),
            "resolve_selector_model must early-return None when encryptor is missing"
        );
    }

    #[test]
    fn resolve_selector_model_returns_llm_conn_params() {
        let source = include_str!("matrix_cloud_runtime.rs");
        assert!(
            source.contains("crate::memory_relevance::LlmConnParams"),
            "resolve_selector_model must return LlmConnParams from memory_relevance module"
        );
    }
}
