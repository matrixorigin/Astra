use super::server::tool_transport::ToolExecutionService;
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

    async fn database_health(&self) -> DatabaseHealth {
        if self.database_healthy().await {
            DatabaseHealth::Connected
        } else {
            DatabaseHealth::Unavailable
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseHealth {
    Connected,
    Unavailable,
    Misconfigured,
}

impl DatabaseHealth {
    pub fn is_healthy(self) -> bool {
        matches!(self, Self::Connected)
    }

    pub fn database_label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Unavailable => "unavailable",
            Self::Misconfigured => "misconfigured",
        }
    }
}

/// Forwards a Memoria API call (already enriched with session_id) to the Memoria backend.
/// Injectable so tests can capture/verify the forwarded body without a real network.
#[async_trait]
pub trait MemoriaForwarder: Send + Sync {
    async fn forward(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String>;

    /// Return readiness from the same transport that owns Memoria requests.
    /// Keeping this on the forwarder prevents startup, request, and health
    /// paths from inventing independent dependency truth.
    async fn health(&self) -> MemoriaHealth;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoriaHealth {
    Connected,
    Unavailable(String),
    Disabled,
}

#[derive(Clone, Debug)]
struct CachedMemoriaHealth {
    value: MemoriaHealth,
    refreshed_at: Option<std::time::Instant>,
}

impl CachedMemoriaHealth {
    fn new(value: MemoriaHealth) -> Self {
        Self {
            value,
            refreshed_at: None,
        }
    }
}

impl MemoriaHealth {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Unavailable(_) => "unavailable",
            Self::Disabled => "disabled",
        }
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

#[derive(Clone)]
pub(crate) struct TurnPersistenceState {
    pub(crate) core_event_writer: Arc<dyn TurnCoreEventWriter>,
    pub(crate) tool_event_writer: Arc<dyn TurnToolEventWriter>,
    pub(crate) hook_db_writer: Arc<dyn TurnHookDbWriter>,
    pub(crate) reflection_state_store: Arc<dyn TurnReflectionStateStore>,
    pub(crate) reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
    pub(crate) observer_worker: Arc<dyn TurnObserverWorker>,
    pub(crate) auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
    pub(crate) session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
}

impl Default for TurnPersistenceState {
    fn default() -> Self {
        Self {
            core_event_writer: Arc::new(NoopTurnCoreEventWriter),
            tool_event_writer: Arc::new(NoopTurnToolEventWriter),
            hook_db_writer: Arc::new(NoopTurnHookDbWriter),
            reflection_state_store: Arc::new(InMemoryTurnReflectionStateStore::default()),
            reflection_lesson_writer: Arc::new(NoopTurnReflectionLessonWriter),
            observer_worker: Arc::new(NoopTurnObserverWorker),
            auxiliary_event_writer: Arc::new(NoopTurnAuxiliaryEventWriter),
            session_activity_writer: Arc::new(NoopTurnSessionActivityWriter),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AdminState {
    pub(crate) authorizer: Arc<dyn AdminAuthorizer>,
    pub(crate) initializer: Arc<dyn AdminInitializer>,
    pub(crate) token_reader: Arc<dyn AdminTokenReader>,
    pub(crate) token_writer: Arc<dyn AdminTokenWriter>,
    pub(crate) audit_reader: Arc<dyn AdminAuditReader>,
    pub(crate) feedback_stats_reader: Arc<dyn AdminFeedbackStatsReader>,
    pub(crate) user_role_manager: Arc<dyn AdminUserRoleManager>,
    pub(crate) config_service: Arc<dyn astra_services::AdminConfigService>,
}

impl Default for AdminState {
    fn default() -> Self {
        Self {
            authorizer: Arc::new(auth::UnconfiguredAdminAuthorizer),
            initializer: Arc::new(auth::UnconfiguredAdminInitializer),
            token_reader: Arc::new(auth::UnconfiguredAdminTokenReader),
            token_writer: Arc::new(auth::UnconfiguredAdminTokenWriter),
            audit_reader: Arc::new(auth::UnconfiguredAdminAuditReader),
            feedback_stats_reader: Arc::new(auth::UnconfiguredAdminFeedbackStatsReader),
            user_role_manager: Arc::new(auth::UnconfiguredAdminUserRoleManager),
            config_service: Arc::new(astra_services::UnconfiguredAdminConfigService),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ExecutionServicesState {
    pub(crate) task_service: Arc<dyn TaskService>,
    pub(crate) edge_registry_service: Arc<dyn EdgeRegistryService>,
    pub(crate) edge_dispatch_service: Arc<dyn EdgeDispatchService>,
    pub(crate) task_lease_service: Arc<dyn TaskLeaseService>,
    pub(crate) run_lifecycle_service: Arc<dyn RunLifecycleService>,
}

impl Default for ExecutionServicesState {
    fn default() -> Self {
        Self {
            task_service: Arc::new(UnconfiguredTaskService),
            edge_registry_service: Arc::new(UnconfiguredEdgeRegistryService),
            edge_dispatch_service: Arc::new(UnconfiguredEdgeDispatchService),
            task_lease_service: Arc::new(UnconfiguredTaskLeaseService),
            run_lifecycle_service: Arc::new(UnconfiguredRunLifecycleService),
        }
    }
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
    pub(crate) harness_service: Arc<dyn HarnessService>,
    pub(crate) sandbox_service: Arc<dyn SandboxService>,
    pub(crate) branch_service: Arc<dyn BranchService>,
    pub(crate) data_versioning_service: Arc<dyn DataVersioningService>,
    pub(crate) marketplace_service: Arc<dyn MarketplaceService>,
    pub(crate) marketplace_stats_service: Arc<dyn MarketplaceStatsService>,
    pub(crate) replay_service: Arc<dyn ReplayService>,
    pub(crate) session_audit_service: Arc<dyn SessionAuditService>,
    pub(crate) skill_service: Arc<dyn SkillService>,
    pub(crate) skill_config_service: Arc<dyn SkillConfigService>,
    pub(crate) mcp_registry_service: Arc<dyn astra_services::McpRegistryService>,
    pub(crate) agent_binding_service: Arc<dyn astra_services::AgentBindingService>,
    pub(crate) llm_trusted_domain_service:
        Arc<dyn astra_services::llm_trusted_domains::LlmTrustedDomainService>,
    pub(crate) evaluation_service: Arc<dyn EvaluationService>,
    pub(crate) introspection_service: Arc<dyn IntrospectionService>,
    pub(crate) reflect_service: Arc<dyn ReflectService>,
    pub(crate) fernet_encryptor: FernetTokenEncryptor,
    pub(crate) turn_persistence: TurnPersistenceState,
    pub(crate) execution: ExecutionServicesState,
    pub(crate) admin: AdminState,
    pub(crate) chat_turn_bridge:
        Option<Arc<crate::turn::bridge::inprocess::InProcessChatTurnBridge>>,
    pub(crate) chat_turn_bridge_secret: String,
    pub(crate) chat_turn_bridge_cache: Arc<tokio::sync::Mutex<SessionCache>>,
    pub memoria_base_url: String,
    pub memoria_master_key: Option<String>,
    pub memoria_forwarder: Arc<dyn MemoriaForwarder>,
    memoria_health_cache: Arc<std::sync::RwLock<CachedMemoriaHealth>>,
    memoria_health_refresh: Arc<tokio::sync::Mutex<()>>,
    pub shared_pool: Option<SharedPool>,
    /// Owner-neutral Matrix pool, journal ingestion, sync persistence, and shutdown tracking.
    pub(crate) matrix_cloud_runtime: Option<Arc<crate::matrix_cloud_runtime::MatrixCloudRuntime>>,
    /// Edge §5.5 callbacks (`/tools/result`, `/approval/respond`); keys via [`astra_turn_core::edge_ledger`].
    pub(crate) edge_callback_ledger:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>>,
    /// Multi-agent profile registry — defines agent tiers, delegation rules.
    pub(crate) agent_profile_registry: Arc<astra_services::AgentProfileRegistry>,
    /// Delegation engine — coordinates multi-agent runs.
    pub(crate) delegation_engine: Option<Arc<crate::server::delegation::engine::DelegationEngine>>,
    /// Team persistence store — CRUD for team definitions and execution history.
    pub(crate) team_store:
        Option<Arc<dyn astra_services::team_persistence::TeamPersistenceService>>,
    /// Per-user resource governor for limit checking and usage tracking (Phase 5).
    pub resource_governor: std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>,
    /// Canonical branch authority. Production always wires the database
    /// adapter; `None` exists only for narrow unit fixtures and cannot
    /// validate an authority-bearing request.
    pub(crate) session_context_coordinator:
        Option<Arc<dyn astra_services::SessionContextCoordinator>>,
    pub(crate) session_handoff_service: Option<Arc<astra_services::DatabaseSessionHandoffService>>,
    pub(crate) session_fork_coordinator:
        Option<Arc<astra_services::DatabaseSessionForkCoordinator>>,
    pub(crate) session_publish_service: Option<Arc<astra_services::DatabaseSessionPublishService>>,
    pub(crate) execution_grant_signer: Option<Arc<astra_services::ExecutionGrantSigner>>,
    pub(crate) session_actor_id: String,
    /// Live edge agent WebSocket connections for remote tool execution (Phase 6).
    pub edge_connection_pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    /// Shared ToolExecutionService for admin-controllable disabled_tool_offers.
    /// All executors share a clone of this service so admin API changes
    /// take effect immediately on in-flight sessions.
    pub tool_execution_service: ToolExecutionService,
    /// Shared HTTP client for upstream LLM proxy requests (completions handler).
    /// Reuses connection pool and TLS state across requests.
    pub(crate) http_client: reqwest::Client,
    /// Cloud-authoritative repository for plan state and step-run history.
    /// Defaults to [`astra_plan::InMemoryPlanRepository`]; production wires
    /// [`astra_plan::CloudPlanRepository`] backed by the MatrixOne pool.
    pub(crate) plan_repo: Arc<dyn astra_plan::PlanRepository>,
    pub(crate) cors_origins: Option<String>,
    /// Prometheus-style metrics registry. Shared across handlers and the
    /// pipeline so /metrics exposes a single source of truth.
    pub(crate) metrics_registry: Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>,
    /// Multi-agent coordination metrics — scraped into metrics_registry on each
    /// /metrics request for a unified exposition endpoint.
    pub(crate) multi_agent_metrics: astra_services::multi_agent::SharedMultiAgentMetrics,
    #[cfg(feature = "harness")]
    pub harness_registry: crate::server::harness::handlers::HarnessSinkRegistry,
}

impl AppState {
    /// Shared §5.5 ledger (`POST /tools/result`, `POST /approval/respond`); same `Arc` as
    /// [`InProcessChatTurnBridge`](crate::turn::bridge::inprocess::InProcessChatTurnBridge) when wired.
    pub fn edge_callback_ledger(
        &self,
    ) -> Arc<tokio::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>> {
        self.edge_callback_ledger.clone()
    }

    pub fn new(service_info: ServiceInfo, health_checker: Arc<dyn HealthChecker>) -> Self {
        let chat_turn_bridge_cache =
            Arc::new(tokio::sync::Mutex::new(SessionCache::new(1000, 86400.0)));
        let default_memoria = astra_core::MemoriaSettings::from_env();
        let edge_connection_pool =
            astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
        let tool_execution_service = ToolExecutionService::builder()
            .edge_connection_pool(edge_connection_pool.clone())
            .build();
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
            harness_service: Arc::new(UnconfiguredHarnessService),
            sandbox_service: Arc::new(UnconfiguredSandboxService),
            branch_service: Arc::new(UnconfiguredBranchService),
            data_versioning_service: Arc::new(UnconfiguredDataVersioningService),
            marketplace_service: Arc::new(UnconfiguredMarketplaceService),
            marketplace_stats_service: Arc::new(NoopMarketplaceStatsService),
            replay_service: Arc::new(UnconfiguredReplayService),
            session_audit_service: Arc::new(UnconfiguredSessionAuditService),
            skill_service: Arc::new(UnconfiguredSkillService),
            skill_config_service: Arc::new(UnconfiguredSkillConfigService),
            mcp_registry_service: Arc::new(astra_services::UnconfiguredMcpRegistryService),
            agent_binding_service: Arc::new(astra_services::UnconfiguredAgentBindingService),
            llm_trusted_domain_service: Arc::new(
                astra_services::llm_trusted_domains::UnconfiguredLlmTrustedDomainService,
            ),
            evaluation_service: Arc::new(UnconfiguredEvaluationService),
            introspection_service: Arc::new(UnconfiguredIntrospectionService),
            reflect_service: Arc::new(UnconfiguredReflectService),
            fernet_encryptor: FernetTokenEncryptor::new("dev-key-not-for-production")
                .or_else(|_| FernetTokenEncryptor::new("0123456789abcdef"))
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        target: "astra_runtime::app_state",
                        error = %e,
                        "fallback encryption init failed; using insecure default key"
                    );
                    // Last resort: use a deterministic key so the app doesn't crash.
                    FernetTokenEncryptor::new("abcdefghijklmnop").expect("hardcoded key must work")
                }),
            turn_persistence: TurnPersistenceState::default(),
            execution: ExecutionServicesState::default(),
            admin: AdminState::default(),
            chat_turn_bridge: None,
            chat_turn_bridge_secret: "dev-bridge-secret-change-me".to_string(),
            chat_turn_bridge_cache,
            memoria_base_url: default_memoria.base_url,
            memoria_master_key: default_memoria.master_key,
            memoria_forwarder: Arc::new(NoopMemoriaForwarder),
            memoria_health_cache: Arc::new(std::sync::RwLock::new(CachedMemoriaHealth::new(
                MemoriaHealth::Disabled,
            ))),
            memoria_health_refresh: Arc::new(tokio::sync::Mutex::new(())),
            shared_pool: None,
            matrix_cloud_runtime: None,
            edge_callback_ledger: Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            agent_profile_registry: Arc::new(astra_services::AgentProfileRegistry::new()),
            delegation_engine: None,
            team_store: None,
            resource_governor: std::sync::Arc::new(
                astra_services::resource_governor::InMemoryResourceGovernor::new(),
            ),
            session_context_coordinator: None,
            session_handoff_service: None,
            session_fork_coordinator: None,
            session_publish_service: None,
            execution_grant_signer: None,
            session_actor_id: std::env::var("ASTRA_POD_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| format!("astra-runtime-{}", uuid::Uuid::new_v4())),
            edge_connection_pool,
            tool_execution_service,
            http_client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("failed to build shared HTTP client"),
            plan_repo: Arc::new(astra_plan::InMemoryPlanRepository::new()),
            cors_origins: None,
            metrics_registry: Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new()),
            multi_agent_metrics: astra_services::multi_agent::shared_metrics(),
            #[cfg(feature = "harness")]
            harness_registry: crate::server::harness::handlers::HarnessSinkRegistry::new(),
        }
    }

    pub fn with_cors_origins(mut self, cors_origins: Option<String>) -> Self {
        self.cors_origins = cors_origins;
        self
    }

    pub fn with_session_context_authority(
        mut self,
        coordinator: Arc<dyn astra_services::SessionContextCoordinator>,
        signer: Arc<astra_services::ExecutionGrantSigner>,
    ) -> Self {
        self.session_context_coordinator = Some(coordinator);
        self.execution_grant_signer = Some(signer);
        self
    }

    pub fn with_session_handoff_service(
        mut self,
        service: Arc<astra_services::DatabaseSessionHandoffService>,
    ) -> Self {
        self.session_handoff_service = Some(service);
        self
    }

    pub fn with_session_fork_coordinator(
        mut self,
        coordinator: Arc<astra_services::DatabaseSessionForkCoordinator>,
    ) -> Self {
        self.session_fork_coordinator = Some(coordinator);
        self
    }

    pub fn with_session_publish_service(
        mut self,
        service: Arc<astra_services::DatabaseSessionPublishService>,
    ) -> Self {
        self.session_publish_service = Some(service);
        self
    }

    /// Inject the plan repository — production wires
    /// [`astra_plan::CloudPlanRepository`]; tests typically keep the default
    /// [`astra_plan::InMemoryPlanRepository`].
    pub fn with_plan_repository(mut self, repo: Arc<dyn astra_plan::PlanRepository>) -> Self {
        self.plan_repo = repo;
        self
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
            Arc::new(ReqwestMemoriaForwarder::new(base_url.clone(), key))
        };
        *astra_core::sync_poison::recover_rwlock_write(&self.memoria_health_cache) =
            CachedMemoriaHealth::new(
                if master_key.as_deref().is_some_and(|key| !key.is_empty()) {
                    MemoriaHealth::Unavailable("probe pending".to_string())
                } else {
                    MemoriaHealth::Disabled
                },
            );
        self.memoria_base_url = base_url;
        self.memoria_master_key = master_key;
        self
    }

    /// Inject a custom MemoriaForwarder (for testing).
    pub fn with_memoria_forwarder(mut self, forwarder: Arc<dyn MemoriaForwarder>) -> Self {
        self.memoria_forwarder = forwarder;
        *astra_core::sync_poison::recover_rwlock_write(&self.memoria_health_cache) =
            CachedMemoriaHealth::new(MemoriaHealth::Unavailable("probe pending".to_string()));
        self
    }

    pub fn cached_memoria_health(&self) -> MemoriaHealth {
        astra_core::sync_poison::recover_rwlock_read(&self.memoria_health_cache)
            .value
            .clone()
    }

    /// Refresh a capability probe at most once per `max_age`. The lock is
    /// rechecked after acquisition, so concurrent callers collapse into one
    /// outbound request instead of serially replaying the same probe.
    pub async fn refresh_memoria_health_if_stale(
        &self,
        max_age: std::time::Duration,
    ) -> MemoriaHealth {
        let _refresh = self.memoria_health_refresh.lock().await;
        {
            let cached = astra_core::sync_poison::recover_rwlock_read(&self.memoria_health_cache);
            if cached.refreshed_at.is_some_and(|at| at.elapsed() < max_age) {
                return cached.value.clone();
            }
        }
        let value = self.memoria_forwarder.health().await;
        *astra_core::sync_poison::recover_rwlock_write(&self.memoria_health_cache) =
            CachedMemoriaHealth {
                value: value.clone(),
                refreshed_at: Some(std::time::Instant::now()),
            };
        value
    }

    pub fn with_admin_authorizer(mut self, admin_authorizer: Arc<dyn AdminAuthorizer>) -> Self {
        self.admin.authorizer = admin_authorizer;
        self
    }

    pub fn with_admin_config_service(
        mut self,
        admin_config_service: Arc<dyn astra_services::AdminConfigService>,
    ) -> Self {
        self.admin.config_service = admin_config_service;
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

    pub fn with_harness_service(mut self, harness_service: Arc<dyn HarnessService>) -> Self {
        self.harness_service = harness_service;
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

    pub fn with_mcp_registry_service(
        mut self,
        mcp_registry_service: Arc<dyn astra_services::McpRegistryService>,
    ) -> Self {
        self.mcp_registry_service = mcp_registry_service;
        self
    }

    pub fn with_agent_binding_service(
        mut self,
        agent_binding_service: Arc<dyn astra_services::AgentBindingService>,
    ) -> Self {
        self.agent_binding_service = agent_binding_service;
        self
    }

    pub fn with_llm_trusted_domain_service(
        mut self,
        service: Arc<dyn astra_services::llm_trusted_domains::LlmTrustedDomainService>,
    ) -> Self {
        self.llm_trusted_domain_service = service;
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

    pub fn with_turn_auxiliary_event_writer(
        mut self,
        turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
    ) -> Self {
        self.turn_persistence.auxiliary_event_writer = turn_auxiliary_event_writer;
        self
    }

    pub fn with_turn_core_event_writer(
        mut self,
        turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
    ) -> Self {
        self.turn_persistence.core_event_writer = turn_core_event_writer;
        self
    }

    pub fn with_turn_tool_event_writer(
        mut self,
        turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
    ) -> Self {
        self.turn_persistence.tool_event_writer = turn_tool_event_writer;
        self
    }

    pub fn with_turn_hook_db_writer(
        mut self,
        turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
    ) -> Self {
        self.turn_persistence.hook_db_writer = turn_hook_db_writer;
        self
    }
    pub fn with_turn_reflection_lesson_writer(
        mut self,
        turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
    ) -> Self {
        self.turn_persistence.reflection_lesson_writer = turn_reflection_lesson_writer;
        self
    }

    pub fn with_turn_reflection_state_store(
        mut self,
        turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
    ) -> Self {
        self.turn_persistence.reflection_state_store = turn_reflection_state_store;
        self
    }

    pub fn with_turn_observer_worker(
        mut self,
        turn_observer_worker: Arc<dyn TurnObserverWorker>,
    ) -> Self {
        self.turn_persistence.observer_worker = turn_observer_worker;
        self
    }

    pub fn with_turn_session_activity_writer(
        mut self,
        turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
    ) -> Self {
        self.turn_persistence.session_activity_writer = turn_session_activity_writer;
        self
    }

    pub fn with_run_lifecycle_service(
        mut self,
        run_lifecycle_service: Arc<dyn RunLifecycleService>,
    ) -> Self {
        self.execution.run_lifecycle_service = run_lifecycle_service;
        self
    }

    pub fn with_task_service(mut self, task_service: Arc<dyn TaskService>) -> Self {
        self.execution.task_service = task_service;
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        edge_registry_service: Arc<dyn EdgeRegistryService>,
    ) -> Self {
        self.execution.edge_registry_service = edge_registry_service;
        self
    }

    pub fn with_edge_dispatch_service(
        mut self,
        edge_dispatch_service: Arc<dyn EdgeDispatchService>,
    ) -> Self {
        self.execution.edge_dispatch_service = edge_dispatch_service;
        self
    }

    pub fn with_tool_execution_service(
        mut self,
        tool_execution_service: ToolExecutionService,
    ) -> Self {
        self.tool_execution_service = tool_execution_service;
        self
    }

    pub fn with_task_lease_service(
        mut self,
        task_lease_service: Arc<dyn TaskLeaseService>,
    ) -> Self {
        self.execution.task_lease_service = task_lease_service;
        self
    }

    pub fn with_admin_initializer(mut self, admin_initializer: Arc<dyn AdminInitializer>) -> Self {
        self.admin.initializer = admin_initializer;
        self
    }

    pub fn with_admin_token_reader(
        mut self,
        admin_token_reader: Arc<dyn AdminTokenReader>,
    ) -> Self {
        self.admin.token_reader = admin_token_reader;
        self
    }

    pub fn with_admin_token_writer(
        mut self,
        admin_token_writer: Arc<dyn AdminTokenWriter>,
    ) -> Self {
        self.admin.token_writer = admin_token_writer;
        self
    }

    pub fn with_admin_audit_reader(
        mut self,
        admin_audit_reader: Arc<dyn AdminAuditReader>,
    ) -> Self {
        self.admin.audit_reader = admin_audit_reader;
        self
    }

    pub fn with_admin_feedback_stats_reader(
        mut self,
        admin_feedback_stats_reader: Arc<dyn AdminFeedbackStatsReader>,
    ) -> Self {
        self.admin.feedback_stats_reader = admin_feedback_stats_reader;
        self
    }

    pub fn with_admin_user_role_manager(
        mut self,
        admin_user_role_manager: Arc<dyn AdminUserRoleManager>,
    ) -> Self {
        self.admin.user_role_manager = admin_user_role_manager;
        self
    }

    pub fn with_chat_turn_bridge(
        mut self,
        chat_turn_bridge: Arc<crate::turn::bridge::inprocess::InProcessChatTurnBridge>,
    ) -> Self {
        self.chat_turn_bridge = Some(chat_turn_bridge);
        self
    }

    pub fn with_chat_turn_bridge_secret(
        mut self,
        chat_turn_bridge_secret: impl Into<String>,
    ) -> Self {
        self.chat_turn_bridge_secret = chat_turn_bridge_secret.into();
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
        engine: Arc<crate::server::delegation::engine::DelegationEngine>,
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

    pub fn with_resource_governor(
        mut self,
        governor: Arc<dyn astra_services::resource_governor::ResourceGovernor>,
    ) -> Self {
        self.resource_governor = governor;
        self
    }

    /// Access the agent profile registry.
    /// Shared Prometheus-style metrics registry. Handlers and pipeline code
    /// register/increment counters here; `/metrics` renders its contents.
    pub fn metrics_registry(&self) -> Arc<astra_turn_core::pipeline_metrics::MetricsRegistry> {
        self.metrics_registry.clone()
    }

    pub fn agent_profile_registry(&self) -> &astra_services::AgentProfileRegistry {
        &self.agent_profile_registry
    }

    /// Access the delegation engine (if configured).
    pub fn delegation_engine(
        &self,
    ) -> Option<&Arc<crate::server::delegation::engine::DelegationEngine>> {
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
    client: reqwest::Client,
    health_timeout: std::time::Duration,
}

impl ReqwestMemoriaForwarder {
    pub fn new(base_url: String, master_key: String) -> Self {
        Self::new_with_timeouts(
            base_url,
            master_key,
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(30),
        )
    }

    /// Build a forwarder with custom connect/request timeouts. Tests use this
    /// to shorten the black-hole timeout from 30s → 200ms without changing
    /// production defaults.
    pub fn new_with_timeouts(
        base_url: String,
        master_key: String,
        connect_timeout: std::time::Duration,
        request_timeout: std::time::Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            .expect("failed to build Memoria HTTP client");
        Self {
            base_url,
            master_key,
            client,
            health_timeout: request_timeout.min(std::time::Duration::from_secs(5)),
        }
    }

    fn request_builder(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, endpoint);
        let mut payload = body.clone();
        let authenticated_user_id = payload
            .as_object_mut()
            .and_then(|object| object.remove("user_id"))
            .and_then(|value| value.as_str().map(str::trim).map(str::to_string))
            .filter(|user_id| !user_id.is_empty());
        let request = self
            .client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.master_key))
            .json(&payload);
        // Astra authenticates the caller before reaching this boundary and
        // overwrites body.user_id. Memoria's master-key mode derives its
        // storage scope from X-User-Id, not from arbitrary request fields.
        // Project the authenticated principal into the transport header, and
        // keep that transport-only identity out of endpoint domain payloads.
        match authenticated_user_id {
            Some(user_id) => request.header("X-User-Id", user_id),
            _ => request,
        }
    }

    async fn bounded_error_body(mut response: reqwest::Response, limit: usize) -> String {
        let mut body = Vec::with_capacity(limit.min(1024));
        while body.len() < limit {
            let Ok(Some(chunk)) = response.chunk().await else {
                break;
            };
            let remaining = limit - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        String::from_utf8_lossy(&body).into_owned()
    }
}

#[async_trait]
impl MemoriaForwarder for ReqwestMemoriaForwarder {
    async fn forward(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let resp = self
            .request_builder(method, endpoint, &body)
            .send()
            .await
            .map_err(|e| format!("Memoria request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = Self::bounded_error_body(resp, 4096).await;
            return Err(format!("Memoria error {status}: {text}"));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| format!("Memoria response read error: {e}"))?;
        if text.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str(&text).map_err(|e| format!("Memoria parse error: {e}"))
    }

    async fn health(&self) -> MemoriaHealth {
        let url = format!("{}/v1/health/analyze", self.base_url.trim_end_matches('/'));
        match self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.master_key))
            .timeout(self.health_timeout)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => MemoriaHealth::Connected,
            Ok(response) => {
                let status = response.status();
                let body = Self::bounded_error_body(response, 1024).await;
                MemoriaHealth::Unavailable(format!("status={status}, body={body}"))
            }
            Err(error) => MemoriaHealth::Unavailable(error.to_string()),
        }
    }
}

/// Disabled transport used when Memoria is not configured.
pub struct NoopMemoriaForwarder;

#[async_trait]
impl MemoriaForwarder for NoopMemoriaForwarder {
    async fn forward(
        &self,
        _method: reqwest::Method,
        _endpoint: &str,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("Memoria not configured on server".to_string())
    }

    async fn health(&self) -> MemoriaHealth {
        MemoriaHealth::Disabled
    }
}

#[derive(Clone, Debug)]
pub struct MatrixOneHealthChecker {
    settings: MatrixOneSettings,
    shared_pool: Option<SharedPool>,
}

impl MatrixOneHealthChecker {
    pub fn new(settings: MatrixOneSettings) -> Self {
        Self {
            settings,
            shared_pool: None,
        }
    }

    pub fn with_pool(mut self, shared_pool: SharedPool) -> Self {
        self.shared_pool = Some(shared_pool);
        self
    }
}

#[async_trait]
impl HealthChecker for MatrixOneHealthChecker {
    async fn database_healthy(&self) -> bool {
        self.database_health().await.is_healthy()
    }

    async fn database_health(&self) -> DatabaseHealth {
        let query_timeout = Duration::from_secs(2);
        if let Some(shared_pool) = &self.shared_pool {
            return match tokio::time::timeout(
                query_timeout,
                query("SELECT 1").execute(shared_pool.get()),
            )
            .await
            {
                Ok(Ok(_)) => DatabaseHealth::Connected,
                Ok(Err(error)) => {
                    tracing::warn!(
                        database = %self.settings.database,
                        %error,
                        "matrixone health probe failed"
                    );
                    DatabaseHealth::Unavailable
                }
                Err(_) => {
                    tracing::warn!(
                        database = %self.settings.database,
                        timeout_secs = query_timeout.as_secs(),
                        "matrixone health probe timed out"
                    );
                    DatabaseHealth::Unavailable
                }
            };
        }
        tracing::warn!(
            database = %self.settings.database,
            "matrixone health check invoked without injected SharedPool"
        );
        DatabaseHealth::Misconfigured
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_local_transport::ServerLocalToolTransport;
    use crate::server::tool_transport::{
        ExecutorBinding, ExecutorStatus, ToolExecutionRequest, ToolTransportKind,
        WorkspaceAuthority, WorkspaceBinding,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct AlwaysHealthy;

    #[async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn matrixone_health_without_pool_is_misconfigured() {
        let checker = MatrixOneHealthChecker::new(MatrixOneSettings::mock());
        assert_eq!(
            checker.database_health().await,
            DatabaseHealth::Misconfigured
        );
        assert!(!checker.database_healthy().await);
    }

    /// audit-A1: ReqwestMemoriaForwarder must set connect_timeout and timeout
    /// so a hung Memoria server cannot block the Axum handler indefinitely.
    /// This test starts a real TCP listener that accepts but never responds,
    /// proving the client times out instead of hanging forever.
    #[tokio::test]
    async fn memoria_forwarder_times_out_on_unresponsive_server() {
        // Black-hole server: accepts connections, never sends a response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                // Hold the connection open, never respond.
                tokio::spawn(async move {
                    let _ = tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    drop(sock);
                });
            }
        });

        // Use a 200ms request timeout so the black-hole behaviour manifests
        // within the test budget; production still uses 30s.
        let forwarder = ReqwestMemoriaForwarder::new_with_timeouts(
            format!("http://{addr}"),
            "test-key".to_string(),
            std::time::Duration::from_millis(500),
            std::time::Duration::from_millis(200),
        );

        let start = std::time::Instant::now();
        let result = forwarder
            .forward(
                reqwest::Method::POST,
                "/v1/memories/retrieve",
                serde_json::json!({"query": "test"}),
            )
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "should fail with timeout, got: {result:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(60),
            "should time out well before 60s, took {elapsed:?}"
        );

        let health_started = std::time::Instant::now();
        let health = forwarder.health().await;
        assert!(matches!(health, MemoriaHealth::Unavailable(_)));
        assert!(
            health_started.elapsed() < std::time::Duration::from_secs(1),
            "readiness must honor the forwarder's bounded timeout"
        );
    }

    #[test]
    fn memoria_forwarder_request_builder_preserves_method_url_and_auth() {
        let forwarder = ReqwestMemoriaForwarder::new_with_timeouts(
            "http://memoria.test".to_string(),
            "test-key".to_string(),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );

        let request = forwarder
            .request_builder(
                reqwest::Method::PUT,
                "/v1/memories/test-id/correct",
                &serde_json::json!({"new_content": "x", "reason": "y"}),
            )
            .build()
            .expect("request builder");

        assert_eq!(request.method(), reqwest::Method::PUT);
        assert_eq!(
            request.url().as_str(),
            "http://memoria.test/v1/memories/test-id/correct"
        );
        assert_eq!(
            request
                .headers()
                .get("Authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer test-key")
        );
    }

    #[tokio::test]
    async fn memoria_forwarder_honors_http_method() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let method = req
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().next())
                    .unwrap_or("UNKNOWN");
                let body = format!(r#"{{"method":"{method}"}}"#);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        });

        let forwarder = ReqwestMemoriaForwarder::new_with_timeouts(
            format!("http://{addr}"),
            "test-key".to_string(),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        let result = forwarder
            .forward(
                reqwest::Method::PUT,
                "/v1/memories/test-id/correct",
                serde_json::json!({"new_content": "x", "reason": "y"}),
            )
            .await
            .expect("forward success");
        assert_eq!(result["method"], "PUT");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn memoria_health_uses_authenticated_readiness_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("GET /v1/health/analyze "), "{request}");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-key"),
                "{request}"
            );
            let body = format!("database absent{}", "x".repeat(8192));
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let forwarder = ReqwestMemoriaForwarder::new_with_timeouts(
            format!("http://{addr}"),
            "test-key".to_string(),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );

        let health = forwarder.health().await;

        assert!(matches!(
            health,
            MemoriaHealth::Unavailable(detail)
                if detail.contains("503")
                    && detail.contains("database absent")
                    && detail.len() < 1200
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn memoria_forwarder_accepts_successful_empty_delete_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("DELETE /v1/memories/memory-1 "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("x-user-id: user-1\r\n"),
                "authenticated owner must be projected to Memoria scope: {request}"
            );
            assert!(
                !request.contains("\"user_id\""),
                "transport identity must not leak into the Memoria domain body: {request}"
            );
            let _ = socket
                .write_all(
                    b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
        });

        let forwarder = ReqwestMemoriaForwarder::new_with_timeouts(
            format!("http://{addr}"),
            "test-key".to_string(),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        let response = forwarder
            .forward(
                reqwest::Method::DELETE,
                "/v1/memories/memory-1",
                serde_json::json!({"user_id": "user-1"}),
            )
            .await
            .expect("204 delete is a confirmed successful operation");
        assert_eq!(response, serde_json::json!({}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn edge_callback_ledger_accessor_shares_backing_store() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy));
        let ledger = state.edge_callback_ledger();
        ledger
            .lock()
            .await
            .insert("req-1".into(), serde_json::json!({ "status": "queued" }));

        let snapshot = state.edge_callback_ledger();
        assert_eq!(
            snapshot.lock().await.get("req-1"),
            Some(&serde_json::json!({ "status": "queued" }))
        );
    }

    struct NoopLocalTransport;

    #[async_trait]
    impl ServerLocalToolTransport for NoopLocalTransport {
        async fn execute_server_local_tool(
            &self,
            request: &ToolExecutionRequest,
            _cancel_token: Option<&CancellationToken>,
        ) -> astra_tools::ToolResult {
            astra_tools::ToolResult::text(format!("local:{}", request.tool_name))
        }
    }

    fn edge_runtime_environment_advertisement(edge_agent_id: &str) -> serde_json::Value {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let binding = astra_runtime_env::RunBinding::resolve(
            astra_runtime_env::WorkspaceBinding::edge_workspace(
                "/Users/test/project",
                astra_runtime_env::WorkspaceAuthority::ReadWrite,
            ),
            astra_runtime_env::ExecutorBinding::edge_agent(edge_agent_id.to_string()),
            astra_runtime_env::RuntimeBinding::host_process(format!("edge-host:{edge_agent_id}")),
            astra_runtime_env::PolicyIntent::local_developer(),
            &registry,
        );
        serde_json::to_value(astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            binding,
        ))
        .expect("edge advertisement serializes")
    }

    #[tokio::test]
    async fn default_tool_execution_service_does_not_bypass_durable_dispatch() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
        state.edge_connection_pool.register_with_capabilities(
            "user-1",
            "edge-selected",
            Some("MacBook Pro".to_string()),
            Some("/Users/test/project".to_string()),
            Some(edge_runtime_environment_advertisement("edge-selected")),
            None,
            tx,
        );

        let service = state.tool_execution_service.clone();
        let request = ToolExecutionRequest {
            user_id: "user-1".to_string(),
            run_id: "run-1".to_string(),
            session_id: "session-1".to_string(),
            turn_chain_id: "chain-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({"cmd": "pwd"}),
            workspace: WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            executor: ExecutorBinding::edge_agent(
                "edge-selected",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            workspace_record: None,
            runtime: None,
            runtime_file_transfer: None,
            runtime_file_transfer_required: false,
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            runtime_filesystem_boundary: None,
            runtime_edge_dispatch_authorization: None,
            runtime_edge_dispatch_authorization_required: false,
            selected_offer: None,
            policy: crate::server::tool_transport::ToolPolicySnapshot::default(),
        };
        let result = service.execute(request, &NoopLocalTransport).await;
        assert!(result.is_error, "{result:?}");
        let metadata = result.metadata.expect("transport metadata");
        assert_eq!(metadata["error_kind"], "transport_unavailable");
        assert_eq!(metadata["execution_started"], false);
        assert_eq!(metadata["side_effects_maybe"], false);
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "a connected socket must not bypass durable dispatch admission"
        );
    }

    #[test]
    fn metrics_registry_accessor_returns_same_arc_and_no_delegation_by_default() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy));
        let first = state.metrics_registry();
        let second = state.metrics_registry();

        assert!(Arc::ptr_eq(&first, &second));
        assert!(state.delegation_engine().is_none());
    }

    #[tokio::test]
    async fn default_memoria_forwarder_is_noop_until_configured() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy));
        let err = state
            .memoria_forwarder
            .forward(
                reqwest::Method::POST,
                "/v1/memories/retrieve",
                serde_json::json!({ "query": "test" }),
            )
            .await
            .expect_err("default AppState should not talk to Memoria");

        assert!(err.contains("Memoria not configured"));
    }
}
