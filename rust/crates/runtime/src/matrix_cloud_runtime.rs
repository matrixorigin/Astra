//! Matrix-backed cloud plumbing in one place: [`SharedPool`], journal ingestion, and
//! [`SyncOrchestrator`] (learning, events, templates, preferences). Used by `mo-agent-server`
//! [`AppState`] and by the CLI [`ReplState`] as a single `Arc` attachment.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mo_agent_core::{MatrixOneSettings, SharedPool};
use mo_agent_services::{
    event_ingestion::{self, IngestionConfig, IngestionEvent},
    session_journal::JournalEvent,
    state_sync::{MatrixOneSyncService, PlanTemplateSyncRow},
    CloudTransport, DomainAdapter, SyncOrchestrator, SyncPolicy,
};

use crate::pipeline::{
    calibration::ProgressiveCalibrator, entity::EntityGraph, persistence::ToolHealthEntry,
    pattern::PatternLibrary,
};
use crate::sync_adapters::{
    EventAdapter, LearningAdapter, MatrixOneTransport, PreferenceAdapter, TemplateAdapter,
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
            .unwrap_or_else(|_| mo_agent_core::DEV_MATRIXONE_PASSWORD.into()),
        database: std::env::var("MATRIXONE_DATABASE").unwrap_or_else(|_| "dev_agent".into()),
    }
}

/// Pool + ingestion + unified sync orchestrator. Safe to share behind `Arc`.
pub struct MatrixCloudRuntime {
    shared_pool: SharedPool,
    ingestion: Mutex<Option<mo_agent_services::event_ingestion::IngestionSender>>,
    sync_orchestrator: Mutex<SyncOrchestrator>,
    /// Edge preference map (same `Arc` as [`PreferenceAdapter`] inside the orchestrator).
    preference_store: Arc<Mutex<BTreeMap<String, String>>>,
    /// Cached plan templates from last `pull_domain(Templates)`.
    template_cache: Arc<Mutex<Vec<PlanTemplateSyncRow>>>,
}

impl MatrixCloudRuntime {
    /// Wire ingestion worker and sync domains to an existing [`SharedPool`].
    pub fn attach(
        shared_pool: SharedPool,
        profile: &str,
        user_id: &str,
        entity_graph: Arc<Mutex<EntityGraph>>,
        pattern_library: Arc<Mutex<PatternLibrary>>,
        calibrator: Arc<Mutex<ProgressiveCalibrator>>,
        tool_health: Arc<Mutex<Vec<ToolHealthEntry>>>,
        cloud_learning_version: Option<i64>,
    ) -> Self {
        let pool = shared_pool.get().clone();
        let (sender, _stats, _jh) =
            event_ingestion::EventIngestionWorker::spawn(pool.clone(), IngestionConfig::default());
        let sync_svc = Arc::new(MatrixOneSyncService::new(pool));
        let transport: Arc<dyn CloudTransport> = Arc::new(MatrixOneTransport::new(
            sync_svc,
            profile.to_string(),
        ));
        let mut orch = SyncOrchestrator::new(transport, user_id.to_string());
        let learning_adapter = LearningAdapter::new(
            entity_graph,
            pattern_library,
            calibrator,
            tool_health,
        );
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

        Self {
            shared_pool,
            ingestion: Mutex::new(Some(sender)),
            sync_orchestrator: Mutex::new(orch),
            preference_store,
            template_cache,
        }
    }

    pub fn preference_store(&self) -> Arc<Mutex<BTreeMap<String, String>>> {
        Arc::clone(&self.preference_store)
    }

    pub fn template_cache(&self) -> Arc<Mutex<Vec<PlanTemplateSyncRow>>> {
        Arc::clone(&self.template_cache)
    }

    pub fn shared_pool(&self) -> &SharedPool {
        &self.shared_pool
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
        if let Ok(mut g) = self.ingestion.lock() {
            if let Some(s) = g.take() {
                s.shutdown();
            }
        }
    }

    pub fn sync_orchestrator_lock(&self) -> std::sync::MutexGuard<'_, SyncOrchestrator> {
        self.sync_orchestrator
            .lock()
            .expect("sync orchestrator mutex poisoned")
    }
}

/// Build a [`SyncOrchestrator`] with all edge sync domains (tests / harness).
pub fn build_sync_orchestrator_with_adapters(
    transport: Arc<dyn CloudTransport>,
    user_id: &str,
    entity_graph: Arc<Mutex<EntityGraph>>,
    pattern_library: Arc<Mutex<PatternLibrary>>,
    calibrator: Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: Arc<Mutex<Vec<ToolHealthEntry>>>,
    ingestion: mo_agent_services::event_ingestion::IngestionSender,
) -> SyncOrchestrator {
    let mut orch = SyncOrchestrator::new(transport, user_id.to_string());
    let learning_adapter = LearningAdapter::new(
        entity_graph,
        pattern_library,
        calibrator,
        tool_health,
    );
    orch.register(Box::new(learning_adapter), SyncPolicy::learning());
    let preference_store = Arc::new(Mutex::new(BTreeMap::new()));
    let template_cache = Arc::new(Mutex::new(Vec::<PlanTemplateSyncRow>::new()));
    orch.register(
        Box::new(EventAdapter::new(ingestion)),
        SyncPolicy::events(),
    );
    orch.register(
        Box::new(TemplateAdapter::new(Arc::clone(&template_cache))),
        SyncPolicy::templates(),
    );
    orch.register(
        Box::new(PreferenceAdapter::new(preference_store)),
        SyncPolicy::preferences(),
    );
    orch
}

#[cfg(test)]
mod tests {
    use super::*;
    use mo_agent_services::{NoopTransport, SyncDomain, event_ingestion::IngestionSender};

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
        );
        let domains: Vec<SyncDomain> = orch.status_summary().into_iter().map(|(d, _)| d).collect();
        assert!(domains.contains(&SyncDomain::Learning));
        assert!(domains.contains(&SyncDomain::Events));
        assert!(domains.contains(&SyncDomain::Templates));
        assert!(domains.contains(&SyncDomain::Preferences));
    }

}
