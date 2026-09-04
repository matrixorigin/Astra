pub mod admin;
pub mod admin_config;
pub mod agent_bindings;
pub mod agent_lessons;
pub mod agents;
pub mod artifact_policy;
pub mod auth;
pub mod branches;
pub mod config_version_cloud;
pub mod context;
pub mod context_manifest;
pub mod coordination;
pub mod data_versioning;
pub mod db_row;
pub mod decisions;
pub mod delegated_findings;
pub mod edge_context;
pub mod evaluation;
pub mod event_ingestion;
pub mod events;
pub mod execution_grant;
pub mod external_artifacts;
pub mod harness;
pub mod inference_execution;
pub mod interaction_contract;
pub mod introspection;
pub mod jobs;
pub mod llm_trusted_domains;
pub mod marketplace;
pub mod marketplace_stats;
pub mod mcp_registry;
pub mod model_request_context;
pub mod models;
pub mod multi_agent;
pub mod pagination;
pub mod personal_skills;
pub mod prompt_delta;
pub mod reflect;
pub(crate) mod registry_payload;
pub use registry_payload::validate_registered_endpoint_url;
pub mod replay;
pub mod resource_governor;
pub mod runs;
pub mod runtime_maintenance;
pub mod sandbox;
pub mod self_surface;
pub mod semantic_read_observation_store;
pub mod service_error;
pub mod session_analytics;
pub mod session_artifact_store;
pub mod session_audit;
pub mod session_checkpoint;
pub mod session_context_coordinator;
pub mod session_fork_coordinator;
pub mod session_handoff;
pub mod session_identity;
pub mod session_journal;
pub(crate) mod session_lifecycle;
pub mod session_memory_inventory;
pub mod session_restore;
pub mod session_workspace;
pub mod skill_auto_route_judge;
pub mod skill_config;
pub mod skills;
pub mod snapshot_sql;
pub mod state_projection;
pub mod state_sync;
pub mod storage;
pub mod sync_engine;
pub mod sync_outbox;
pub mod team_persistence;
pub mod tool_invocation_ledger;
pub mod triggers;
pub mod turn_intent_judge;
pub mod verification;
pub mod weighted_admission;
pub mod work;
pub mod workflows;
pub mod workspace_records;

fn missing_shared_pool_error(
    service_name: &'static str,
    settings: &astra_core::MatrixOneSettings,
) -> sqlx::Error {
    sqlx::Error::Configuration(Box::new(std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        format!(
            "{service_name} requires an injected SharedPool for database '{}'; wire .with_pool(...) at composition time instead of relying on implicit fallback connections",
            settings.database
        ),
    )))
}

fn require_shared_pool(
    pool: Option<&astra_core::SharedPool>,
    service_name: &'static str,
    settings: &astra_core::MatrixOneSettings,
) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
    pool.map(|pool| pool.get().clone())
        .ok_or_else(|| missing_shared_pool_error(service_name, settings))
}

fn require_shared_pool_message(
    pool: Option<&astra_core::SharedPool>,
    service_name: &'static str,
    settings: &astra_core::MatrixOneSettings,
) -> Result<sqlx::Pool<sqlx::MySql>, String> {
    require_shared_pool(pool, service_name, settings).map_err(|err| err.to_string())
}

pub use admin::{
    AdminAuditFilter, AdminAuditReader, AdminAuditRecord, AdminAuthorizer,
    AdminFeedbackStatsFilter, AdminFeedbackStatsReader, AdminFeedbackStatsRecord, AdminInitRecord,
    AdminInitializer, AdminTokenCreateRequestData, AdminTokenFilter, AdminTokenReader,
    AdminTokenRecord, AdminTokenWriter, AdminUserRoleManager, AdminUserRoleRecord,
    AdminUserRoleRequestData, AuthenticatedUser,
};
pub use admin_config::{
    ADMIN_CONFIG_ALLOWED_KEYS, ADMIN_CONFIG_KEY_REASONING_OFFERING, AdminConfigService,
    DatabaseAdminConfigService, UnconfiguredAdminConfigService,
};
pub use agent_bindings::{
    AgentBindingCreateRequestData, AgentBindingOwnerScope, AgentBindingPayload, AgentBindingRecord,
    AgentBindingService, AgentBindingStatus, DatabaseAgentBindingService,
    InMemoryAgentBindingService, UnconfiguredAgentBindingService, validate_agent_binding_create,
    validate_agent_binding_payload,
};
pub use agent_lessons::{
    Lesson, LessonHint, LessonKind, MIN_LESSON_ACTION_CHARS, MIN_LESSON_ACTION_WORDS, NewLesson,
    SCRATCHPAD_LOWERCASE_PHRASES, is_action_prompt_worthy, memory_value_to_lesson_hint,
    sanitize_for_prompt,
};
pub use agents::{
    AgentCreateRequestData, AgentListItem, AgentListRecord, AgentRecord, AgentService,
    AgentUpdateRequestData, DatabaseAgentService, InMemoryAgentService, UnconfiguredAgentService,
};
pub use artifact_policy::{
    PresignedArtifactDownload, artifact_download_signature, build_presigned_artifact_download,
};
pub use astra_core::composite_snapshot::{
    CompositeSnapshot, CompositeSnapshotIndex, DataSnapshotRef, MemorySnapshotRef, SnapshotRef,
    SnapshotSpec,
};
pub use auth::{
    AuthLoginRequestData, AuthPrincipal, AuthPrincipalOrigin, AuthProviderAuthorizedRequestContext,
    AuthRefreshRequestData, AuthRegisterRequestData, AuthService, AuthTokenRecord, AuthUserRecord,
    DatabaseAdminAuditReader, DatabaseAdminAuthorizer, DatabaseAdminFeedbackStatsReader,
    DatabaseAdminInitializer, DatabaseAdminTokenReader, DatabaseAdminTokenWriter,
    DatabaseAdminUserRoleManager, DatabaseAuthService, DatabaseSessionService, EdgeTokenBinding,
    ExternalAuthProviderConfig, ExternalAuthorizeRequestData, ExternalAuthorizedRequest,
    ExternalCatalogResponse, ExternalLoginRequestData, ExternalProviderClient,
    ExternalProviderPublicRecord, ExternalRequestDescriptor, ExternalRuntimeContextRequestData,
    ExternalRuntimeContextResponse, ExternalSessionRecord, FernetTokenEncryptor,
    HttpExternalProviderClient, ProviderRequestDescriptor, ReauthenticationProofRecord,
    ReauthenticationPurpose, ReauthenticationRequestData, SessionCreateRequestData,
    SessionListFilter, SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
    VerifiedIdentityLoginRequestData,
};
pub use branches::{BranchService, DatabaseBranchService, UnconfiguredBranchService};
pub use context::{
    ContextService, DatabaseContextService, SnapshotCreateRequestData, SnapshotListFilter,
    SnapshotListItem, SnapshotListRecord, SnapshotRecord, UnconfiguredContextService,
};
pub use context_manifest::{
    BASELINE_PREVIEW_TEMPLATES, BENCHMARK_TOOL_PREVIEW_BUDGET, BUDGET_V1_8K_PROMPT_CAP,
    BUDGET_V1_8K_TOTAL_CAP, BudgetV1_8k, CONTEXT_MANIFEST_REASONS, ConfidenceAction,
    ContextManifestError, ContextManifestItemWrite, ContextManifestWrite,
    DELEGATION_BLOCKER_ZONE_CAP, DELEGATION_ZONE_CAP, DatabaseContextManifestStore,
    DelegationBudget, DelegationBudgetAllocation, RECENT_TAIL_BENCHMARK_FLOOR, RenderMode,
    RetrievalStage, TURN_INTENT_BENCHMARK_COMPARISON, TurnIntentBudgetAllocation,
    artifact_id_from_raw_ref, budget_for_turn_intent, content_hash_with_normalize_version,
    cross_session_retrieval_requires_user_filter, delegation_budget, delegation_budget_allocation,
    expired_artifact_placeholder, next_action_confidence_action, suggested_next_action_expires_at,
};
pub use coordination::{
    AgentProfile, AgentProfileRegistry, AgentResult, AgentTier, AgentTrigger, AggregationStrategy,
    CoordinationPattern, DelegationRequest, DelegationResult, PipelineStage, aggregate_results,
};
pub use data_versioning::{
    DataVersioningService, DatabaseDataVersioningService, UnconfiguredDataVersioningService,
};
pub use decisions::{
    DatabaseDecisionService, DecisionCreateRequestData, DecisionListFilter, DecisionListRecord,
    DecisionRecord, DecisionService, DecisionWithContextRecord, UnconfiguredDecisionService,
};
pub use edge_context::{EdgeContext, EdgeProfile, EdgeSkillRef};
pub use evaluation::{DatabaseEvaluationService, EvaluationService, UnconfiguredEvaluationService};
pub use events::{
    DatabaseEventService, EventCreateRequestData, EventIngestionSource, EventListFilter,
    EventListRecord, EventRecord, EventService, UnconfiguredEventService,
};
pub use execution_grant::{ExecutionGrantError, ExecutionGrantSigner};
pub use harness::{
    DatabaseHarnessService, HarnessCitationRecord, HarnessDecisionRequest, HarnessItemRecord,
    HarnessNodeCatalogRecord, HarnessRunRecord, HarnessService, HarnessSkillDraftRecord,
    HarnessSkillRuleRecord, HarnessTemplateRecord, SkillifyAgentCitation, SkillifyAgentDraft,
    SkillifyAgentExecutor, SkillifyAgentOutput, SkillifyAgentRequest, SkillifyAgentRule,
    SkillifyDraftRecord, SkillifyDraftRequest, SkillifyPublishRecord, SkillifyPublishRequest,
    SkillifyRunRequest, SkillifySourceFile, SkillifySourcePacket, UnconfiguredHarnessService,
};
pub use inference_execution::{
    InferenceCanonicalTransitionReceipt, InferenceInvocationAdmissionResolution,
    InferenceInvocationInput, InferenceInvocationPlan, InferenceInvocationTerminal,
    InferenceProviderAttemptPlan, InferenceProviderDeliveryState, InferenceProviderWireIdentity,
    InferenceRunAdmissionAuthority, InferenceSettlementReconcileOutcome, InferenceTerminalStatus,
    InferenceUsage, InferenceUsageStatus, admit_inference_invocation,
    admit_inference_invocation_with_first_provider_attempt, begin_inference_provider_attempt,
    declare_inference_attempt_settlement, declare_inference_settlement,
    finish_inference_invocation, finish_inference_provider_attempt,
    load_inference_canonical_transitions_for_session, next_inference_logical_attempt_pair_base,
    plan_inference_invocation, plan_inference_provider_attempt,
    plan_inference_provider_attempt_with_context, reconcile_inference_settlement,
    reconcile_inference_settlements, renew_inference_invocation_owner,
    retire_inference_canonical_transitions_through_turn, settle_uncertain_inference_admission,
};
pub use interaction_contract::{
    InteractionContract, InteractionDurableStore, InteractionIdentity, InteractionKind,
    InteractionStatus, approval_decision_status, ask_user_response_status, edge_dispatch_status,
};
pub use introspection::{
    DatabaseIntrospectionService, IntrospectionService, UnconfiguredIntrospectionService,
};
pub use jobs::{
    InMemoryJobService, JobRecord, JobService, JobSubmitRequestData, UnconfiguredJobService,
};
pub use llm_trusted_domains::{
    DatabaseLlmTrustedDomainService, LlmTrustedDomainDeleteResponse, LlmTrustedDomainRecord,
    LlmTrustedDomainService, LlmTrustedDomainUpsertRequest, LlmTrustedDomainUpsertRequestData,
    UnconfiguredLlmTrustedDomainService,
};
pub use marketplace::{
    DatabaseMarketplaceService, MarketplaceService, UnconfiguredMarketplaceService,
};
pub use marketplace_stats::{
    DatabaseMarketplaceStatsService, MarketplaceStatsService, NoopMarketplaceStatsService,
    QualityReportData, SkillMarketplaceStats, SkillSearchQuery, SkillSearchResponse,
    SkillSearchResult,
};
pub use mcp_registry::{
    DatabaseMcpRegistryService, McpBindingRequestData, McpDiscoveredToolData, McpRegisterRecord,
    McpRegisterRequestData, McpRegisteredBindingRecord, McpRegisteredToolRecord,
    McpRegistryService, McpRuntimeBindingRecord, McpServerRequestData,
    UnconfiguredMcpRegistryService, mcp_binding_tool_namespace, mcp_schema_hash,
};
pub use model_request_context::{
    MODEL_REQUEST_CONTEXT_SCHEMA, ModelRequestBudget, ModelRequestCache, ModelRequestCompaction,
    ModelRequestComposition, ModelRequestContextEvent, ModelRequestContextRecord,
    ModelRequestContextSeed, ModelRequestEventStage, ModelRequestIdentity, ModelRequestLineage,
    ModelRequestMetricsRow, ModelRequestRolloutStage, ModelRequestTopology,
    ModelRequestTraceCoverage, ModelRequestUsage, ModelRequestWireComposition,
    aggregate_model_request_metrics, list_model_request_context_events,
    model_request_trace_coverage,
};
pub use models::{
    AdmittedModelExecution, DatabaseModelService, DeclaredModelAccess, ModelAccessAction,
    ModelAccessAvailability, ModelAccessKind, ModelAccessProjectionResponse, ModelAccessReason,
    ModelAccessStatus, ModelAccessViewResponse, ModelCreateRequestData, ModelDefaultCandidate,
    ModelDefaultInvalidReason, ModelDefaultResolution, ModelDefaultScope, ModelDefaultSource,
    ModelExecutionPlacement, ModelListCursor, ModelListItem, ModelListItemResponse, ModelListPage,
    ModelListPageResponse, ModelOfferingResolutionError, ModelRecord, ModelService,
    ModelUpdateRequestData, PricingData, PromptCacheCapabilityData, PromptCacheProtocolData,
    PromptCacheReuseScopeData, PromptCacheVolatileDeliveryData, PromptCacheVolatilePlacementData,
    QuirksData, ResolvedActiveLlmModel, ResolvedModelOffering, UnconfiguredModelService,
    model_catalog_revision, project_model_access, project_model_access_page,
    project_model_access_page_with_default_catalog, project_model_access_with_default,
    prompt_cache_capability_from_models_yaml, resolve_active_llm_model,
    resolve_active_llm_offering, resolve_memory_offerings, resolve_reasoning_offering,
    revalidate_active_llm_offering, validate_model_offering_id,
};
pub use multi_agent::{
    DatabaseEdgeDispatchService, DatabaseEdgeRegistryService, EdgeAgentRecord,
    EdgeDispatchIdentity, EdgeDispatchRow, EdgeDispatchService, EdgeRegistrationLease,
    EdgeRegistryService, HeartbeatError, UnconfiguredEdgeDispatchService,
    UnconfiguredEdgeRegistryService,
};
pub use pagination::{
    MAX_ADMIN_AUDIT_LOG_LIMIT, MAX_API_LIST_LIMIT, MAX_API_LIST_OFFSET, clamp_admin_audit_limit,
    clamp_api_list_pagination,
};
pub use personal_skills::{
    ActivateUserSkillVersion, ActivePersonalSkillRecord, CreateUserSkillSource,
    DatabasePersonalSkillStore, PersonalSkillError, RecordUserSkillEvaluation,
    SKILL_MD_NORMALIZE_VERSION, SubmitUserSkillVersion, UserSkillEvaluationRecord,
    UserSkillSourceRecord, UserSkillVersionRecord, normalize_skill_md, skill_md_content_hash,
};
pub use prompt_delta::{
    PromptDeltaCounts, PromptRequestObservability, PromptRequestPersistInput,
    PromptRequestPersistResult, PromptRequestPlan, PromptRequestPlanInput,
    count_prompt_requests_for_run, count_prompt_requests_for_session,
    load_latest_prompt_observability_for_run, load_latest_prompt_observability_for_session,
    persist_prompt_request, plan_prompt_request,
};
pub use reflect::{
    DatabaseReflectService, ReflectReport, ReflectService, UnconfiguredReflectService,
};
pub use replay::{DatabaseReplayService, ReplayService, UnconfiguredReplayService};
pub use runs::{
    CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, DatabaseRunStateStore,
    DurableRunListPage, DurableRunRecord, InMemoryRunStateStore, RunContinuationRecord,
    RunLifecycleService, RunListCursor, RunListRecord, RunMutationDisposition, RunMutationRecord,
    RunStateStore, RunStatusRecord, UnconfiguredRunLifecycleService, WorkItemRuntimeBindingRequest,
    WorkRuntimeBindingRequest, extract_event_type, run_list_cursor_db_updated_at,
    run_list_cursor_run_id, transform_run_event_for_client, validate_run_list_limit,
};
pub use sandbox::{
    DatabaseSandboxService, SandboxCreateRequestData, SandboxRecord, SandboxService,
    UnconfiguredSandboxService,
};
pub use self_surface::{
    AcceptanceSurface, BudgetConfig, BudgetState, BudgetSurface, CapabilitySurface,
    EnvironmentSurface, EventPreview, EvolutionRecord, EvolutionSurface, GoalSurface,
    HealthSurface, JournalSurface, LocalSelfSurfaceService, NoopSelfSurfaceRuntimeSupport,
    PersistentSelfSnapshot, ProfileSurface, RunSurface, RunTotals, SelfSurfaceCheck,
    SelfSurfaceDimension, SelfSurfaceResponse, SelfSurfaceRuntimeSupport, SelfSurfaceService,
    SignalsSurface, StepRecord, SurfaceConstraints, ToolCallView, ToolFailureView, ToolHealthView,
    TraceSurface, VerificationSurface,
};
pub use service_error::{ServiceError, ServiceErrorKind, ServiceResult};
pub use session_artifact_store::{
    DatabaseSessionArtifactStore, LOCAL_SESSION_LAYOUT_VERSION, LocalSessionArtifactStore,
    MUTABLE_ARTIFACT_PROJECTION_ID_PREFIX, OwnerScope, OwnerScopeKind, SessionArtifactJsonRecord,
    SessionArtifactJsonStore, SessionArtifactListCursor, SessionArtifactListPage,
    SessionArtifactReference, SessionArtifactReferenceKind, SessionArtifactStore,
    SessionArtifactStoreError, StoredSessionArtifact, configure_local_owner_scope,
    local_owner_scope, local_owner_user_id, local_session_artifact_store,
};
pub use session_context_coordinator::{
    AcquireWriterOutcome, DatabaseSessionContextCoordinator, MaterializedConversationV1,
    RenewedTurnAuthority, ReserveTurnOutcome, SessionAuthorityEventV1, SessionContextCoordinator,
    SessionContextCoordinatorError, TransferWriterOutcome, WriterTransferConflictV1,
    WriterTransferRequestV1,
};
pub use session_fork_coordinator::{
    DatabaseSessionForkCoordinator, PrepareSessionForkV1, SessionForkCoordinatorError,
};
pub use session_handoff::{
    AttachSessionOutcomeV1, AttachSessionRequestV1, ClaimSessionControllerOutcomeV1,
    DatabaseSessionHandoffService, FenceSessionWriterOutcomeV1, HandoffTransitionPatchV1,
    ReleaseSessionControllerOutcomeV1, RequestSessionHandoffV1, SessionControllerBasisV1,
    SessionHandoffError, SessionHandoffEventV1, TransitionSessionHandoffV1,
};
pub use session_identity::{MAX_PERSISTED_SESSION_ID_BYTES, validate_persisted_session_id};
pub use skill_auto_route_judge::{
    SkillAutoRouteCandidate, SkillAutoRouteJudge, SkillAutoRouteJudgeContext,
    SkillAutoRouteJudgeError, build_skill_auto_route_prompt, parse_skill_auto_route_response,
    skill_auto_route_judge_messages,
};
pub use skill_config::{
    DatabaseSkillConfigService, SkillConfigService, UnconfiguredSkillConfigService,
};
pub use skills::{
    DatabaseSkillService, SkillPublishRequestData, SkillRecord, SkillService,
    UnconfiguredSkillService,
};
pub use state_projection::{
    BubbleUpTarget, COMPACTION_INVARIANT_SQL, CompactionInvariant, DatabaseStateProjectionStore,
    DelegationProjectionUpsert, PROTECTED_COMPACTION_CATEGORIES, SkillActivationLlmProbe,
    StateItemUpsert, StateProjectionError, UserAnchorMemoryItem, validate_state_mutation,
};
pub use state_sync::{
    LocalOnlySyncService, MatrixOneSyncService, StateSyncService, SyncDirection, SyncResult,
    SyncStatus,
};
pub use storage::{
    CleanupResult, RetentionPolicy, cleanup_expired_data, database_user_from_row,
    ensure_core_schema, log_session_audit, resolve_active_skill_versions, session_record_from_row,
    update_turn_skill_selection_version,
};
pub use sync_engine::{
    CloudTransport, DomainAdapter, DomainSyncResult, MergeResult, NoopTransport, PayloadFormat,
    PullResult, PullTrigger, PushResult, PushTrigger, SyncDomain, SyncEnvelope, SyncError,
    SyncEvent, SyncOperation, SyncOrchestrator, SyncPayload, SyncPolicy, SyncState, SyncStats,
};
pub use sync_outbox::{
    SYNC_OUTBOX_ACK_TOMBSTONE_RETAINED_RECORDS, SYNC_OUTBOX_ACKED_RETAINED_RECORDS,
    SYNC_OUTBOX_IN_FLIGHT_LEASE_MS, SYNC_OUTBOX_MAX_ATTEMPTS, SYNC_OUTBOX_SCHEMA_VERSION,
    SYNC_OUTBOX_SKIPPED_RETAINED_RECORDS, SyncOutboxAckOutcome, SyncOutboxAckTombstone,
    SyncOutboxDeliverySettlement, SyncOutboxEnqueueOutcome, SyncOutboxFile,
    SyncOutboxJournalBatchOutcome, SyncOutboxJournalDelta, SyncOutboxJournalDeltaOutcome,
    SyncOutboxPoisonKind, SyncOutboxRecord, SyncOutboxRecordState, SyncOutboxSettlementReport,
    SyncOutboxSkipKind, SyncOutboxSkippedRecord, SyncOutboxStatus, SyncOutboxStore,
    sync_outbox_canonical_payload_hash, sync_outbox_stable_event_id,
};
pub use triggers::{
    DatabaseTriggerService, TriggerCreateRequestData, TriggerRecord, TriggerService,
    UnconfiguredTriggerService, WebhookFireData,
};
pub use turn_intent_judge::{
    TurnIntentJudge, TurnIntentJudgeContext, TurnIntentJudgeError, WorkAdmissionActivation,
    WorkAdmissionCapability, WorkAdmissionDecision, WorkAdmissionGraphMutation, WorkAdmissionTask,
    WorkExecutionTopology, build_turn_intent_prompt, parse_turn_intent_response,
    parse_work_admission_response, turn_intent_judge_messages, work_admission_judge_messages,
};
pub use verification::{
    LlmJudge, SubtaskVerificationReport, VerificationCriterion, VerificationResult,
    VerificationRunner, VerifierKind,
};
pub use weighted_admission::{
    AdmissionWork, DatabaseWeightedAdmissionController, DistributedAdmissionError,
    DistributedAdmissionPermit, DistributedAdmissionReservation, WeightedAdmissionController,
    WeightedAdmissionError, WeightedAdmissionLimits, WeightedAdmissionPermit,
};
pub use workflows::{
    UnconfiguredWorkflowService, WorkflowDefRecord, WorkflowListItem, WorkflowRunRecord,
    WorkflowService,
};
pub use workspace_records::{
    DatabaseWorkspaceRecordStore, InMemoryWorkspaceRecordStore, WorkspaceCleanupDebtEntry,
    WorkspaceCleanupDebtStore, WorkspaceCleanupDebtStoreError, WorkspaceRecordEntry,
    WorkspaceRecordStore, WorkspaceRecordStoreError, WorkspaceStateStore,
};
