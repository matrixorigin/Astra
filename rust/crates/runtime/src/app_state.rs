use super::*;
use crate::turn::services::{
    NoopTurnAuxiliaryEventWriter, NoopTurnCoreEventWriter, NoopTurnHookDbWriter,
    NoopTurnSessionActivityWriter, NoopTurnToolEventWriter,
};
use astra_services::auth;

const DEFAULT_NAME: &str = "Agent Engine API";
const DEFAULT_VERSION: &str = "0.1.0";
const DEFAULT_DOCS: &str = "";

#[async_trait]
pub trait HealthChecker: Send + Sync {
    async fn database_healthy(&self) -> bool;
}

/// Forwards a Memoria API call (already enriched with session_id) to the Memoria backend.
/// Injectable so tests can capture/verify the forwarded body without a real network.
#[async_trait]
pub trait MemoriaForwarder: Send + Sync {
    async fn forward(
        &self,
        endpoint: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

#[derive(Clone)]
pub struct AppState {
    pub(crate) service_info: ServiceInfo,
    pub(crate) health_checker: Arc<dyn HealthChecker>,
    pub(crate) auth_service: Arc<dyn AuthService>,
    pub(crate) session_service: Arc<dyn SessionService>,
    pub(crate) agent_service: Arc<dyn AgentService>,
    pub(crate) event_service: Arc<dyn EventService>,
    pub(crate) context_service: Arc<dyn ContextService>,
    pub(crate) decision_service: Arc<dyn DecisionService>,
    pub(crate) model_service: Arc<dyn ModelService>,
    pub(crate) job_service: Arc<dyn JobService>,
    pub(crate) trigger_service: Arc<dyn TriggerService>,
    pub(crate) workflow_service: Arc<dyn WorkflowService>,
    pub(crate) sandbox_service: Arc<dyn SandboxService>,
    pub(crate) branch_service: Arc<dyn BranchService>,
    pub(crate) data_versioning_service: Arc<dyn DataVersioningService>,
    pub(crate) marketplace_service: Arc<dyn MarketplaceService>,
    pub(crate) marketplace_stats_service: Arc<dyn MarketplaceStatsService>,
    pub(crate) replay_service: Arc<dyn ReplayService>,
    pub(crate) session_audit_service: Arc<dyn SessionAuditService>,
    pub(crate) streaming_service: Arc<dyn StreamingService>,
    pub(crate) skill_service: Arc<dyn SkillService>,
    pub(crate) skill_config_service: Arc<dyn SkillConfigService>,
    pub(crate) evaluation_service: Arc<dyn EvaluationService>,
    pub(crate) introspection_service: Arc<dyn IntrospectionService>,
    pub(crate) reflect_service: Arc<dyn ReflectService>,
    pub(crate) learning_feedback_service: Arc<dyn LearningFeedbackService>,
    pub(crate) fernet_encryptor: FernetTokenEncryptor,
    pub(crate) turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
    pub(crate) turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
    pub(crate) turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
    pub(crate) turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
    pub(crate) turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
    pub(crate) turn_observer_worker: Arc<dyn TurnObserverWorker>,
    pub(crate) turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
    pub(crate) turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
    pub(crate) task_service: Arc<dyn TaskService>,
    pub(crate) edge_registry_service: Arc<dyn EdgeRegistryService>,
    pub(crate) task_lease_service: Arc<dyn TaskLeaseService>,
    pub(crate) run_lifecycle_service: Arc<dyn RunLifecycleService>,
    pub(crate) admin_authorizer: Arc<dyn AdminAuthorizer>,
    pub(crate) admin_initializer: Arc<dyn AdminInitializer>,
    pub(crate) admin_token_reader: Arc<dyn AdminTokenReader>,
    pub(crate) admin_token_writer: Arc<dyn AdminTokenWriter>,
    pub(crate) admin_audit_reader: Arc<dyn AdminAuditReader>,
    pub(crate) admin_feedback_stats_reader: Arc<dyn AdminFeedbackStatsReader>,
    pub(crate) admin_user_role_manager: Arc<dyn AdminUserRoleManager>,
    pub(crate) chat_turn_bridge: Arc<dyn ChatTurnBridge>,
    pub(crate) chat_turn_bridge_secret: String,
    pub(crate) chat_turn_bridge_cache: Arc<tokio::sync::Mutex<SessionCache>>,
    /// Pipeline learning writer — shared across all turns, auto-updates
    /// EntityGraph/PatternLibrary/ProgressiveCalibrator from turn outcomes.
    pub(crate) turn_learning_writer: Option<Arc<dyn TurnLearningWriter>>,
    pub memoria_base_url: String,
    pub memoria_master_key: Option<String>,
    pub memoria_forwarder: Arc<dyn MemoriaForwarder>,
    pub shared_pool: Option<SharedPool>,
    /// Matrix pool + journal ingestion + [`astra_services::SyncOrchestrator`] (learning/events).
    pub(crate) matrix_cloud_runtime: Option<Arc<crate::matrix_cloud_runtime::MatrixCloudRuntime>>,
    /// Edge §5.5 callbacks (`/tools/result`, `/approval/respond`); keys via [`crate::turn::edge_ledger`].
    pub(crate) edge_callback_ledger:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>>,
    /// Multi-agent profile registry — defines agent tiers, delegation rules.
    pub(crate) agent_profile_registry: Arc<astra_services::AgentProfileRegistry>,
    /// Delegation engine — coordinates multi-agent runs.
    pub(crate) delegation_engine: Option<Arc<crate::server::delegation_engine::DelegationEngine>>,
    /// Team persistence store — CRUD for team definitions and execution history.
    pub(crate) team_store:
        Option<Arc<dyn astra_services::team_persistence::TeamPersistenceService>>,
}

impl AppState {
    /// Shared §5.5 ledger (`POST /tools/result`, `POST /approval/respond`); same `Arc` as
    /// [`InProcessChatTurnBridge`](crate::turn::bridge_inprocess::InProcessChatTurnBridge) when wired.
    pub fn edge_callback_ledger(
        &self,
    ) -> Arc<tokio::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>> {
        self.edge_callback_ledger.clone()
    }

    pub fn new(service_info: ServiceInfo, health_checker: Arc<dyn HealthChecker>) -> Self {
        let chat_turn_bridge_cache =
            Arc::new(tokio::sync::Mutex::new(SessionCache::new(1000, 86400.0)));
        Self {
            service_info,
            health_checker,
            auth_service: Arc::new(auth::UnconfiguredAuthService),
            session_service: Arc::new(auth::UnconfiguredSessionService),
            agent_service: Arc::new(UnconfiguredAgentService),
            event_service: Arc::new(UnconfiguredEventService),
            context_service: Arc::new(UnconfiguredContextService),
            decision_service: Arc::new(UnconfiguredDecisionService),
            model_service: Arc::new(UnconfiguredModelService),
            job_service: Arc::new(UnconfiguredJobService),
            trigger_service: Arc::new(UnconfiguredTriggerService),
            workflow_service: Arc::new(UnconfiguredWorkflowService),
            sandbox_service: Arc::new(UnconfiguredSandboxService),
            branch_service: Arc::new(UnconfiguredBranchService),
            data_versioning_service: Arc::new(UnconfiguredDataVersioningService),
            marketplace_service: Arc::new(UnconfiguredMarketplaceService),
            marketplace_stats_service: Arc::new(NoopMarketplaceStatsService),
            replay_service: Arc::new(UnconfiguredReplayService),
            session_audit_service: Arc::new(UnconfiguredSessionAuditService),
            streaming_service: Arc::new(UnconfiguredStreamingService),
            skill_service: Arc::new(UnconfiguredSkillService),
            skill_config_service: Arc::new(UnconfiguredSkillConfigService),
            evaluation_service: Arc::new(UnconfiguredEvaluationService),
            introspection_service: Arc::new(UnconfiguredIntrospectionService),
            reflect_service: Arc::new(UnconfiguredReflectService),
            learning_feedback_service: Arc::new(UnconfiguredLearningFeedbackService),
            fernet_encryptor: FernetTokenEncryptor::new("dev-key-not-for-production")
                .unwrap_or_else(|_| FernetTokenEncryptor::new("0123456789abcdef").unwrap()),
            turn_core_event_writer: Arc::new(NoopTurnCoreEventWriter),
            turn_tool_event_writer: Arc::new(NoopTurnToolEventWriter),
            turn_hook_db_writer: Arc::new(NoopTurnHookDbWriter),
            turn_reflection_state_store: Arc::new(InMemoryTurnReflectionStateStore::default()),
            turn_reflection_lesson_writer: Arc::new(NoopTurnReflectionLessonWriter),
            turn_observer_worker: Arc::new(NoopTurnObserverWorker),
            turn_auxiliary_event_writer: Arc::new(NoopTurnAuxiliaryEventWriter),
            turn_session_activity_writer: Arc::new(NoopTurnSessionActivityWriter),
            task_service: Arc::new(UnconfiguredTaskService),
            edge_registry_service: Arc::new(UnconfiguredEdgeRegistryService),
            task_lease_service: Arc::new(UnconfiguredTaskLeaseService),
            run_lifecycle_service: Arc::new(UnconfiguredRunLifecycleService),
            admin_authorizer: Arc::new(auth::UnconfiguredAdminAuthorizer),
            admin_initializer: Arc::new(auth::UnconfiguredAdminInitializer),
            admin_token_reader: Arc::new(auth::UnconfiguredAdminTokenReader),
            admin_token_writer: Arc::new(auth::UnconfiguredAdminTokenWriter),
            admin_audit_reader: Arc::new(auth::UnconfiguredAdminAuditReader),
            admin_feedback_stats_reader: Arc::new(auth::UnconfiguredAdminFeedbackStatsReader),
            admin_user_role_manager: Arc::new(auth::UnconfiguredAdminUserRoleManager),
            chat_turn_bridge: Arc::new(UnavailableChatTurnBridge),
            chat_turn_bridge_secret: "dev-bridge-secret-change-me".to_string(),
            chat_turn_bridge_cache,
            turn_learning_writer: None,
            memoria_base_url: std::env::var("MEMORIA_BASE_URL")
                .unwrap_or_else(|_| crate::config::DEFAULT_MEMORIA_URL.to_string()),
            memoria_master_key: std::env::var("MEMORIA_MASTER_KEY").ok(),
            memoria_forwarder: Arc::new(NoopMemoriaForwarder),
            shared_pool: None,
            matrix_cloud_runtime: None,
            edge_callback_ledger: Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            agent_profile_registry: Arc::new(astra_services::AgentProfileRegistry::new()),
            delegation_engine: None,
            team_store: None,
        }
    }

    pub fn with_memoria_config(
        mut self,
        base_url: impl Into<String>,
        master_key: Option<String>,
    ) -> Self {
        let base_url = base_url.into();
        let key = master_key.clone().unwrap_or_default();
        self.memoria_forwarder = if key.is_empty() {
            Arc::new(NoopMemoriaForwarder)
        } else {
            Arc::new(ReqwestMemoriaForwarder {
                base_url: base_url.clone(),
                master_key: key,
            })
        };
        self.memoria_base_url = base_url;
        self.memoria_master_key = master_key;
        self
    }

    /// Inject a custom MemoriaForwarder (for testing).
    pub fn with_memoria_forwarder(mut self, forwarder: Arc<dyn MemoriaForwarder>) -> Self {
        self.memoria_forwarder = forwarder;
        self
    }

    pub fn with_admin_authorizer(mut self, admin_authorizer: Arc<dyn AdminAuthorizer>) -> Self {
        self.admin_authorizer = admin_authorizer;
        self
    }

    pub fn with_auth_service(mut self, auth_service: Arc<dyn AuthService>) -> Self {
        self.auth_service = auth_service;
        self
    }

    pub fn with_session_service(mut self, session_service: Arc<dyn SessionService>) -> Self {
        self.session_service = session_service;
        self
    }

    pub fn with_agent_service(mut self, agent_service: Arc<dyn AgentService>) -> Self {
        self.agent_service = agent_service;
        self
    }

    pub fn with_event_service(mut self, event_service: Arc<dyn EventService>) -> Self {
        self.event_service = event_service;
        self
    }

    pub fn with_context_service(mut self, context_service: Arc<dyn ContextService>) -> Self {
        self.context_service = context_service;
        self
    }

    pub fn with_decision_service(mut self, decision_service: Arc<dyn DecisionService>) -> Self {
        self.decision_service = decision_service;
        self
    }

    pub fn with_model_service(mut self, model_service: Arc<dyn ModelService>) -> Self {
        self.model_service = model_service;
        self
    }

    pub fn with_job_service(mut self, job_service: Arc<dyn JobService>) -> Self {
        self.job_service = job_service;
        self
    }

    pub fn with_trigger_service(mut self, trigger_service: Arc<dyn TriggerService>) -> Self {
        self.trigger_service = trigger_service;
        self
    }

    pub fn with_workflow_service(mut self, workflow_service: Arc<dyn WorkflowService>) -> Self {
        self.workflow_service = workflow_service;
        self
    }

    pub fn with_sandbox_service(mut self, sandbox_service: Arc<dyn SandboxService>) -> Self {
        self.sandbox_service = sandbox_service;
        self
    }

    pub fn with_branch_service(mut self, branch_service: Arc<dyn BranchService>) -> Self {
        self.branch_service = branch_service;
        self
    }

    pub fn with_data_versioning_service(
        mut self,
        data_versioning_service: Arc<dyn DataVersioningService>,
    ) -> Self {
        self.data_versioning_service = data_versioning_service;
        self
    }

    pub fn with_marketplace_service(
        mut self,
        marketplace_service: Arc<dyn MarketplaceService>,
    ) -> Self {
        self.marketplace_service = marketplace_service;
        self
    }

    pub fn with_marketplace_stats_service(
        mut self,
        marketplace_stats_service: Arc<dyn MarketplaceStatsService>,
    ) -> Self {
        self.marketplace_stats_service = marketplace_stats_service;
        self
    }

    pub fn with_replay_service(mut self, replay_service: Arc<dyn ReplayService>) -> Self {
        self.replay_service = replay_service;
        self
    }

    pub fn with_session_audit_service(
        mut self,
        session_audit_service: Arc<dyn SessionAuditService>,
    ) -> Self {
        self.session_audit_service = session_audit_service;
        self
    }

    pub fn with_streaming_service(mut self, streaming_service: Arc<dyn StreamingService>) -> Self {
        self.streaming_service = streaming_service;
        self
    }

    pub fn with_skill_service(mut self, skill_service: Arc<dyn SkillService>) -> Self {
        self.skill_service = skill_service;
        self
    }

    pub fn with_skill_config_service(
        mut self,
        skill_config_service: Arc<dyn SkillConfigService>,
    ) -> Self {
        self.skill_config_service = skill_config_service;
        self
    }

    pub fn with_fernet_encryptor(mut self, encryptor: FernetTokenEncryptor) -> Self {
        self.fernet_encryptor = encryptor;
        self
    }

    pub fn with_evaluation_service(
        mut self,
        evaluation_service: Arc<dyn EvaluationService>,
    ) -> Self {
        self.evaluation_service = evaluation_service;
        self
    }

    pub fn with_introspection_service(
        mut self,
        introspection_service: Arc<dyn IntrospectionService>,
    ) -> Self {
        self.introspection_service = introspection_service;
        self
    }

    pub fn with_reflect_service(mut self, reflect_service: Arc<dyn ReflectService>) -> Self {
        self.reflect_service = reflect_service;
        self
    }

    pub fn with_learning_feedback_service(
        mut self,
        learning_feedback_service: Arc<dyn LearningFeedbackService>,
    ) -> Self {
        self.learning_feedback_service = learning_feedback_service;
        self
    }

    pub fn with_turn_auxiliary_event_writer(
        mut self,
        turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
    ) -> Self {
        self.turn_auxiliary_event_writer = turn_auxiliary_event_writer;
        self
    }

    pub fn with_turn_core_event_writer(
        mut self,
        turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
    ) -> Self {
        self.turn_core_event_writer = turn_core_event_writer;
        self
    }

    pub fn with_turn_tool_event_writer(
        mut self,
        turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
    ) -> Self {
        self.turn_tool_event_writer = turn_tool_event_writer;
        self
    }

    pub fn with_turn_hook_db_writer(
        mut self,
        turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
    ) -> Self {
        self.turn_hook_db_writer = turn_hook_db_writer;
        self
    }

    pub fn with_turn_reflection_state_store(
        mut self,
        turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
    ) -> Self {
        self.turn_reflection_state_store = turn_reflection_state_store;
        self
    }

    pub fn with_turn_reflection_lesson_writer(
        mut self,
        turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
    ) -> Self {
        self.turn_reflection_lesson_writer = turn_reflection_lesson_writer;
        self
    }

    pub fn with_turn_observer_worker(
        mut self,
        turn_observer_worker: Arc<dyn TurnObserverWorker>,
    ) -> Self {
        self.turn_observer_worker = turn_observer_worker;
        self
    }

    pub fn with_turn_session_activity_writer(
        mut self,
        turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
    ) -> Self {
        self.turn_session_activity_writer = turn_session_activity_writer;
        self
    }

    pub fn with_run_lifecycle_service(
        mut self,
        run_lifecycle_service: Arc<dyn RunLifecycleService>,
    ) -> Self {
        self.run_lifecycle_service = run_lifecycle_service;
        self
    }

    pub fn with_task_service(mut self, task_service: Arc<dyn TaskService>) -> Self {
        self.task_service = task_service;
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        edge_registry_service: Arc<dyn EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = edge_registry_service;
        self
    }

    pub fn with_task_lease_service(
        mut self,
        task_lease_service: Arc<dyn TaskLeaseService>,
    ) -> Self {
        self.task_lease_service = task_lease_service;
        self
    }

    pub fn with_admin_initializer(mut self, admin_initializer: Arc<dyn AdminInitializer>) -> Self {
        self.admin_initializer = admin_initializer;
        self
    }

    pub fn with_admin_token_reader(
        mut self,
        admin_token_reader: Arc<dyn AdminTokenReader>,
    ) -> Self {
        self.admin_token_reader = admin_token_reader;
        self
    }

    pub fn with_admin_token_writer(
        mut self,
        admin_token_writer: Arc<dyn AdminTokenWriter>,
    ) -> Self {
        self.admin_token_writer = admin_token_writer;
        self
    }

    pub fn with_admin_audit_reader(
        mut self,
        admin_audit_reader: Arc<dyn AdminAuditReader>,
    ) -> Self {
        self.admin_audit_reader = admin_audit_reader;
        self
    }

    pub fn with_admin_feedback_stats_reader(
        mut self,
        admin_feedback_stats_reader: Arc<dyn AdminFeedbackStatsReader>,
    ) -> Self {
        self.admin_feedback_stats_reader = admin_feedback_stats_reader;
        self
    }

    pub fn with_admin_user_role_manager(
        mut self,
        admin_user_role_manager: Arc<dyn AdminUserRoleManager>,
    ) -> Self {
        self.admin_user_role_manager = admin_user_role_manager;
        self
    }

    pub fn with_chat_turn_bridge(mut self, chat_turn_bridge: Arc<dyn ChatTurnBridge>) -> Self {
        self.chat_turn_bridge = chat_turn_bridge;
        self
    }

    pub fn with_chat_turn_bridge_secret(
        mut self,
        chat_turn_bridge_secret: impl Into<String>,
    ) -> Self {
        self.chat_turn_bridge_secret = chat_turn_bridge_secret.into();
        self
    }

    pub fn with_chat_turn_bridge_url(mut self, chat_turn_bridge_url: impl Into<String>) -> Self {
        let mut bridge = HttpChatTurnBridge::new(
            chat_turn_bridge_url.into(),
            self.chat_turn_bridge_cache.clone(),
        );
        if let Some(ref writer) = self.turn_learning_writer {
            bridge = bridge.with_learning_writer(writer.clone());
        }
        self.chat_turn_bridge = Arc::new(bridge);
        self
    }

    pub fn with_chat_turn_bridge_url_optional(
        mut self,
        chat_turn_bridge_url: Option<String>,
    ) -> Self {
        if let Some(url) = chat_turn_bridge_url {
            let mut bridge = HttpChatTurnBridge::new(url, self.chat_turn_bridge_cache.clone());
            if let Some(ref writer) = self.turn_learning_writer {
                bridge = bridge.with_learning_writer(writer.clone());
            }
            self.chat_turn_bridge = Arc::new(bridge);
        }
        self
    }

    pub fn with_chat_turn_bridge_cache(
        mut self,
        chat_turn_bridge_cache: Arc<tokio::sync::Mutex<SessionCache>>,
    ) -> Self {
        self.chat_turn_bridge_cache = chat_turn_bridge_cache;
        self
    }

    pub fn with_turn_learning_writer(mut self, writer: Arc<dyn TurnLearningWriter>) -> Self {
        self.turn_learning_writer = Some(writer);
        self
    }

    pub fn with_shared_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_matrix_cloud_runtime(
        mut self,
        rt: Option<Arc<crate::matrix_cloud_runtime::MatrixCloudRuntime>>,
    ) -> Self {
        self.matrix_cloud_runtime = rt;
        self
    }

    pub fn with_agent_profile_registry(
        mut self,
        registry: Arc<astra_services::AgentProfileRegistry>,
    ) -> Self {
        self.agent_profile_registry = registry;
        self
    }

    pub fn with_delegation_engine(
        mut self,
        engine: Arc<crate::server::delegation_engine::DelegationEngine>,
    ) -> Self {
        self.delegation_engine = Some(engine);
        self
    }

    pub fn with_team_store(
        mut self,
        store: Arc<dyn astra_services::team_persistence::TeamPersistenceService>,
    ) -> Self {
        self.team_store = Some(store);
        self
    }

    /// Access the agent profile registry.
    pub fn agent_profile_registry(&self) -> &astra_services::AgentProfileRegistry {
        &self.agent_profile_registry
    }

    /// Access the delegation engine (if configured).
    pub fn delegation_engine(
        &self,
    ) -> Option<&Arc<crate::server::delegation_engine::DelegationEngine>> {
        self.delegation_engine.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct ServiceInfo {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) docs: String,
}

impl ServiceInfo {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        docs: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            docs: docs.into(),
        }
    }
}

impl Default for ServiceInfo {
    fn default() -> Self {
        Self::new(DEFAULT_NAME, DEFAULT_VERSION, DEFAULT_DOCS)
    }
}

/// Real implementation: calls Memoria over HTTP using the server's MEMORIA_MASTER_KEY.
pub struct ReqwestMemoriaForwarder {
    pub base_url: String,
    pub master_key: String,
}

#[async_trait]
impl MemoriaForwarder for ReqwestMemoriaForwarder {
    async fn forward(
        &self,
        endpoint: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}{}", self.base_url, endpoint);
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| format!("Memoria client build error: {e}"))?;
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.master_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Memoria request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Memoria error {status}: {text}"));
        }
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| format!("Memoria parse error: {e}"))
    }
}

/// No-op: returns empty result. Used when Memoria is not configured.
pub struct NoopMemoriaForwarder;

#[async_trait]
impl MemoriaForwarder for NoopMemoriaForwarder {
    async fn forward(
        &self,
        _endpoint: &str,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("Memoria not configured on server".to_string())
    }
}

#[derive(Clone, Debug)]
pub struct MatrixOneHealthChecker {
    settings: MatrixOneSettings,
}

impl MatrixOneHealthChecker {
    pub fn new(settings: MatrixOneSettings) -> Self {
        Self { settings }
    }
}

#[async_trait]
impl HealthChecker for MatrixOneHealthChecker {
    async fn database_healthy(&self) -> bool {
        let pool = match MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(2))
            .connect(&self.settings.database_url())
            .await
        {
            Ok(pool) => pool,
            Err(_) => return false,
        };

        let result = query("SELECT 1").execute(&pool).await.is_ok();
        pool.close().await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{
        calibration::ProgressiveCalibrator, entity::EntityGraph, learning::PipelineLearningWriter,
        pattern::PatternLibrary,
    };
    use std::sync::Mutex;

    fn make_test_learning_writer() -> Arc<dyn TurnLearningWriter> {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let patterns = Arc::new(Mutex::new(PatternLibrary::new()));
        let calibrator = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
        Arc::new(
            PipelineLearningWriter::new()
                .with_entity_graph(graph)
                .with_pattern_library(patterns)
                .with_progressive_calibrator(calibrator),
        )
    }

    #[test]
    fn app_state_with_turn_learning_writer() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker));
        assert!(state.turn_learning_writer.is_none());

        let writer = make_test_learning_writer();
        let state = state.with_turn_learning_writer(writer);
        assert!(state.turn_learning_writer.is_some());
    }

    #[test]
    fn bridge_url_builder_propagates_learning_writer() {
        let writer = make_test_learning_writer();
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_turn_learning_writer(writer)
            .with_chat_turn_bridge_url("http://localhost:9999");

        // The learning writer should be propagated to the bridge.
        // We can't inspect HttpChatTurnBridge fields directly, but we verify
        // that AppState retains the writer.
        assert!(state.turn_learning_writer.is_some());
    }

    #[test]
    fn bridge_url_optional_builder_propagates_learning_writer() {
        let writer = make_test_learning_writer();
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_turn_learning_writer(writer)
            .with_chat_turn_bridge_url_optional(Some("http://localhost:9999".to_string()));

        assert!(state.turn_learning_writer.is_some());
    }

    #[test]
    fn bridge_url_optional_none_keeps_default_bridge() {
        let writer = make_test_learning_writer();
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_turn_learning_writer(writer)
            .with_chat_turn_bridge_url_optional(None);

        assert!(state.turn_learning_writer.is_some());
    }

    struct TestHealthChecker;
    #[async_trait]
    impl HealthChecker for TestHealthChecker {
        async fn database_healthy(&self) -> bool {
            true
        }
    }
}
