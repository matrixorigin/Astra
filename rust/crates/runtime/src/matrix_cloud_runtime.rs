//! Matrix-backed cloud plumbing in one place: [`SharedPool`], journal ingestion, and
//! [`SyncOrchestrator`] (learning, events, templates, preferences). Used by `mo-agent-server`
//! [`AppState`] and by the CLI [`ReplState`] as a single `Arc` attachment.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

use astra_core::{MatrixOneSettings, SharedPool};
use astra_services::{
    CloudTransport, DomainAdapter, SyncOrchestrator, SyncPolicy, TaskLeaseHoldCache, TaskRecord,
    event_ingestion::{self, IngestionConfig, IngestionEvent},
    session_journal::JournalEvent,
    state_sync::{MatrixOneSyncService, PlanTemplateSyncRow},
};

use crate::pipeline::{
    calibration::ProgressiveCalibrator, entity::EntityGraph, pattern::PatternLibrary,
    persistence::ToolHealthEntry,
};
use crate::sync_adapters::{
    EventAdapter, LearningAdapter, MatrixOneTransport, PreferenceAdapter, TaskAdapter,
    TemplateAdapter,
};

/// Environment-driven MatrixOne settings (same defaults as legacy CLI `try_init_ingestion`).
pub fn matrix_settings_from_env() -> MatrixOneSettings {
    MatrixOneSettings {
        host: std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("MATRIXONE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6001),
        user: std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
        password: std::env::var("MATRIXONE_PASSWORD")
            .unwrap_or_else(|_| astra_core::DEV_MATRIXONE_PASSWORD.into()),
        database: std::env::var("MATRIXONE_DATABASE").unwrap_or_else(|_| "astra_runtime".into()),
    }
}

/// Pool + ingestion + unified sync orchestrator. Safe to share behind `Arc`.
pub struct MatrixCloudRuntime {
    shared_pool: SharedPool,
    ingestion: Mutex<Option<astra_services::event_ingestion::IngestionSender>>,
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
        let edge_agent_id: Arc<str> = std::env::var("MO_EDGE_AGENT_ID")
            .unwrap_or_else(|_| "mo-server".into())
            .into();
        let task_mirror = Arc::new(Mutex::new(BTreeMap::new()));
        let task_dirty = Arc::new(Mutex::new(HashSet::new()));

        let pool = shared_pool.get().clone();
        let (sender, _stats, _jh) =
            event_ingestion::EventIngestionWorker::spawn(pool.clone(), IngestionConfig::default());
        let sync_svc = Arc::new(MatrixOneSyncService::new(pool));
        let transport: Arc<dyn CloudTransport> = Arc::new(MatrixOneTransport::new(
            sync_svc,
            profile.to_string(),
            edge_agent_id.as_ref(),
        ));
        let mut orch = SyncOrchestrator::new(transport, user_id.to_string());
        let learning_adapter =
            LearningAdapter::new(entity_graph, pattern_library, calibrator, tool_health);
        if let Some(v) = cloud_learning_version {
            let mut env = learning_adapter.envelope();
            env.mark_pulled(v as u64);
            learning_adapter.set_envelope(env);
        }
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
            sync_orchestrator: TokioMutex::new(orch),
            preference_store,
            template_cache,
            lease_hold_cache,
            task_mirror,
            task_dirty,
            edge_agent_id,
        }
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

    /// Task mirror backing [`crate::sync_adapters::TaskAdapter`] (pull merge + export source).
    pub fn task_sync_mirror(&self) -> Arc<Mutex<BTreeMap<String, TaskRecord>>> {
        Arc::clone(&self.task_mirror)
    }

    /// Dirty task IDs for lease-filtered task sync export.
    pub fn task_sync_dirty(&self) -> Arc<Mutex<HashSet<String>>> {
        Arc::clone(&self.task_dirty)
    }

    pub fn shared_pool(&self) -> &SharedPool {
        &self.shared_pool
    }

    /// Create a [`CloudLlmJudge`] backed by this runtime's database pool.
    ///
    /// Returns `None` if cloud LLM environment variables are not configured.
    /// The judge persists evaluation results directly to the `task_verification_results` table.
    pub fn create_cloud_llm_judge(&self) -> Option<astra_services::CloudLlmJudge> {
        let config = astra_services::CloudLlmConfig::from_env()?;
        let pool = self.shared_pool.get().clone();
        Some(astra_services::CloudLlmJudge::new(config, Some(pool)))
    }

    /// Clone the ingestion sender for use in other subsystems (e.g., durable task lifecycle).
    /// Returns `None` if ingestion is shut down or lock is poisoned.
    pub fn clone_ingestion_sender(
        &self,
    ) -> Option<astra_services::event_ingestion::IngestionSender> {
        self.ingestion.lock().ok()?.as_ref().cloned()
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

    /// Flush and stop the ingestion worker.
    pub fn shutdown_ingestion(&self) {
        if let Ok(mut g) = self.ingestion.lock()
            && let Some(s) = g.take()
        {
            s.shutdown();
        }
    }

    pub async fn sync_orchestrator_lock(&self) -> tokio::sync::MutexGuard<'_, SyncOrchestrator> {
        self.sync_orchestrator.lock().await
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
        let s = matrix_settings_from_env();
        assert!(!s.host.is_empty());
        assert!(s.port > 0);
        assert!(!s.database.is_empty());
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
}
