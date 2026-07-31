use super::*;
use crate::server::deployment_tool_policy::{
    DeploymentToolPolicy, apply_deployment_tool_policy, load_deployment_tool_policy,
};
use crate::server::tool_transport::ToolExecutionService;

pub(super) fn build_auth_service(
    settings: &AppSettings,
    shared_pool: &SharedPool,
    shared_encryptor: &Arc<FernetTokenEncryptor>,
) -> Result<Arc<dyn AuthService>, Box<dyn std::error::Error>> {
    Ok(Arc::new(
        DatabaseAuthService::new(settings.matrixone.clone(), settings.jwt.clone())
            .with_pool(shared_pool.clone())
            .with_encryptor(shared_encryptor.as_ref().clone())
            .with_provider_request_auth(settings.provider_request_auth.clone()),
    ))
}

pub(super) fn build_core_state(
    settings: &AppSettings,
    shared_pool: &SharedPool,
    shared_encryptor: &Arc<FernetTokenEncryptor>,
    auth_service: Arc<dyn AuthService>,
) -> AppState {
    use sha2::{Digest, Sha256};

    let mut execution_grant_key = Sha256::new();
    execution_grant_key.update(b"astra.execution-grant.server-key.v1\0");
    execution_grant_key.update(settings.bridge_secret.as_bytes());
    let execution_grant_signer =
        astra_services::ExecutionGrantSigner::new(execution_grant_key.finalize().as_slice())
            .expect("SHA-256 derived execution grant key has the required length");

    AppState::new(
        ServiceInfo::default(),
        Arc::new(
            MatrixOneHealthChecker::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
        ),
    )
    .with_cors_origins(settings.api.cors_origins.clone())
    .with_shared_pool(shared_pool.clone())
    .with_session_context_authority(
        Arc::new(astra_services::DatabaseSessionContextCoordinator::new(
            shared_pool.clone(),
        )),
        Arc::new(execution_grant_signer),
    )
    .with_plan_repository(Arc::new(astra_plan::CloudPlanRepository::new(
        shared_pool.get().clone(),
    )))
    .with_auth_service(auth_service)
    .with_session_service(Arc::new(
        DatabaseSessionService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_agent_service(Arc::new(
        DatabaseAgentService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_event_service(Arc::new(
        DatabaseEventService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_context_service(Arc::new(
        DatabaseContextService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_decision_service(Arc::new(
        DatabaseDecisionService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_model_service(Arc::new(
        DatabaseModelService::new(settings.matrixone.clone(), Arc::clone(shared_encryptor))
            .with_pool(shared_pool.clone()),
    ))
    .with_job_service(Arc::new(InMemoryJobService::new()))
    .with_trigger_service(Arc::new(
        DatabaseTriggerService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_workflow_service(Arc::new(UnconfiguredWorkflowService))
    .with_sandbox_service(Arc::new(
        DatabaseSandboxService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_branch_service(Arc::new(
        DatabaseBranchService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_data_versioning_service(Arc::new(
        DatabaseDataVersioningService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_marketplace_service(Arc::new(
        DatabaseMarketplaceService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_marketplace_stats_service(Arc::new(
        DatabaseMarketplaceStatsService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_replay_service(Arc::new(
        DatabaseReplayService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_session_audit_service(Arc::new(
        DatabaseSessionAuditService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_skill_service(Arc::new(
        DatabaseSkillService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_skill_config_service(Arc::new(
        DatabaseSkillConfigService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_mcp_registry_service(Arc::new(
        DatabaseMcpRegistryService::new(settings.matrixone.clone(), Arc::clone(shared_encryptor))
            .with_pool(shared_pool.clone()),
    ))
    .with_agent_binding_service(Arc::new(
        astra_services::DatabaseAgentBindingService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_llm_trusted_domain_service(Arc::new(
        astra_services::DatabaseLlmTrustedDomainService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_fernet_encryptor(shared_encryptor.as_ref().clone())
    .with_evaluation_service(Arc::new(
        DatabaseEvaluationService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone())
            .with_memoria_config(
                settings.memoria.base_url.clone(),
                settings.memoria.master_key.clone(),
            ),
    ))
    .with_introspection_service(Arc::new(
        DatabaseIntrospectionService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_reflect_service(Arc::new(
        DatabaseReflectService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
}

pub(super) fn install_turn_persistence_services(
    state: AppState,
    settings: &AppSettings,
    shared_pool: &SharedPool,
) -> AppState {
    state
        .with_turn_core_event_writer(Arc::new(
            DatabaseTurnCoreEventWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_turn_tool_event_writer(Arc::new(
            DatabaseTurnToolEventWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_turn_hook_db_writer(Arc::new(
            DatabaseTurnHookDbWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_turn_reflection_lesson_writer(Arc::new(DatabaseTurnReflectionLessonWriter::new(
            settings.memoria.base_url.clone(),
            settings.memoria.master_key.clone(),
        )))
        .with_turn_observer_worker(Arc::new(DatabaseTurnObserverWorker::new(
            settings.memoria.base_url.clone(),
            settings.memoria.master_key.clone(),
        )))
        .with_turn_auxiliary_event_writer(Arc::new(
            DatabaseTurnAuxiliaryEventWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_turn_session_activity_writer(Arc::new(
            DatabaseTurnSessionActivityWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
}

pub(super) fn install_admin_services(
    state: AppState,
    settings: &AppSettings,
    shared_pool: &SharedPool,
) -> Result<AppState, Box<dyn std::error::Error>> {
    Ok(state
        .with_admin_authorizer(Arc::new(
            DatabaseAdminAuthorizer::new(settings.matrixone.clone(), settings.jwt.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_initializer(Arc::new(
            DatabaseAdminInitializer::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_token_writer(Arc::new(
            DatabaseAdminTokenWriter::from_env(settings.matrixone.clone())
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_token_reader(Arc::new(
            DatabaseAdminTokenReader::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_audit_reader(Arc::new(
            DatabaseAdminAuditReader::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_feedback_stats_reader(Arc::new(
            DatabaseAdminFeedbackStatsReader::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_user_role_manager(Arc::new(
            DatabaseAdminUserRoleManager::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_config_service(Arc::new(
            astra_services::DatabaseAdminConfigService::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        )))
}

pub(super) fn install_execution_services(
    state: AppState,
    shared_pool: &SharedPool,
    lease_hold_cache: &Arc<TaskLeaseHoldCache>,
) -> AppState {
    let metrics = state.multi_agent_metrics.clone();
    let edge_registry_service: Arc<dyn EdgeRegistryService> = Arc::new(
        DatabaseEdgeRegistryService::from_shared(shared_pool).with_metrics(metrics.clone()),
    );
    let edge_dispatch_service: Arc<dyn EdgeDispatchService> = Arc::new(
        DatabaseEdgeDispatchService::from_shared(shared_pool).with_metrics(metrics.clone()),
    );
    let tool_execution_service = build_shared_tool_execution_service(
        state.edge_connection_pool.clone(),
        Arc::clone(&edge_dispatch_service),
        Arc::clone(&edge_registry_service),
        &load_deployment_tool_policy(),
    );
    state
        .with_task_service(Arc::new(MatrixOneTaskService::from_shared(shared_pool)))
        .with_edge_registry_service(edge_registry_service)
        .with_edge_dispatch_service(edge_dispatch_service)
        .with_task_lease_service(Arc::new(
            DatabaseTaskLeaseService::from_shared(shared_pool, Arc::clone(lease_hold_cache))
                .with_metrics(metrics),
        ))
        .with_tool_execution_service(tool_execution_service)
}

fn build_shared_tool_execution_service(
    edge_connection_pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    edge_dispatch_service: Arc<dyn EdgeDispatchService>,
    edge_registry_service: Arc<dyn EdgeRegistryService>,
    policy: &DeploymentToolPolicy,
) -> ToolExecutionService {
    let mut builder = ToolExecutionService::builder()
        .edge_connection_pool(edge_connection_pool)
        .edge_dispatch_service(edge_dispatch_service)
        .edge_registry_service(edge_registry_service);
    if !policy.provider_capabilities.is_empty() {
        builder = builder.initial_provider_capabilities(policy.provider_capabilities.clone());
    }
    if !policy.disabled_tool_offers.is_empty() {
        builder = builder.initial_disabled_tool_offers(&policy.disabled_tool_offers);
    }
    if !policy.provider_allowed_tools.is_empty() {
        builder = builder.initial_provider_allowed_tools(policy.provider_allowed_tools.clone());
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use crate::server::tool_local_transport::ServerLocalToolTransport;
    use crate::server::tool_transport::{
        ExecutorBinding, ExecutorStatus, ToolExecutionRequest, ToolTransportKind,
        WorkspaceAuthority, WorkspaceBinding,
    };

    struct NoopLocalTransport;

    #[derive(Default)]
    struct AdmittingEdgeDispatch {
        admissions: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl EdgeDispatchService for AdmittingEdgeDispatch {
        async fn insert_dispatch(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _payload_json: &str,
        ) -> Result<(), String> {
            self.admissions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }

        async fn poll_pending(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
            Ok(Vec::new())
        }

        async fn deliver_result(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn fail_dispatch(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _reason: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn wait_result(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _timeout: std::time::Duration,
        ) -> Result<Option<String>, String> {
            Ok(None)
        }

        async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
            Ok(0)
        }
    }

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

    fn edge_request() -> ToolExecutionRequest {
        ToolExecutionRequest {
            user_id: "user-1".to_string(),
            run_id: "run-1".to_string(),
            turn_chain_id: "chain-1".to_string(),
            session_id: "session-1".to_string(),
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
            selected_offer: None,
            policy: crate::server::tool_transport::ToolPolicySnapshot::default(),
        }
    }

    #[tokio::test]
    async fn shared_tool_execution_service_wires_edge_websocket_transport() {
        let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
        pool.register_with_capabilities(
            "user-1",
            "edge-selected",
            Some("MacBook Pro".to_string()),
            Some("/Users/test/project".to_string()),
            Some(edge_runtime_environment_advertisement("edge-selected")),
            None,
            tx,
        );

        let dispatch = Arc::new(AdmittingEdgeDispatch::default());
        let service = build_shared_tool_execution_service(
            pool.clone(),
            dispatch.clone(),
            Arc::new(astra_services::multi_agent::UnconfiguredEdgeRegistryService),
            &DeploymentToolPolicy::default(),
        );
        let request = edge_request();
        let handle =
            tokio::spawn(async move { service.execute(request, &NoopLocalTransport).await });

        let message = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("durably admitted edge request is delivered without hanging")
            .expect("edge tool request is delivered");
        let (request_id, delivery_generation) = match message {
            astra_server_types::EdgeServerMessage::ToolRequest {
                request_id,
                tool,
                delivery_generation,
                ..
            } => {
                assert_eq!(tool, "bash");
                (request_id, delivery_generation)
            }
            other => panic!("expected edge tool request, got {other:?}"),
        };
        assert!(pool.deliver_tool_result(
            "user-1",
            "edge-selected",
            &request_id,
            delivery_generation,
            astra_server_types::edge_connection_pool::EdgeToolResult {
                output: "ok".to_string(),
                is_error: false,
                duration_ms: Some(1),
                tool_result_fields: None,
            },
        ));

        let result = handle.await.expect("edge execution task joins");
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "ok");
        assert_eq!(
            dispatch
                .admissions
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "socket delivery must be preceded by exactly one durable admission"
        );
    }
}
