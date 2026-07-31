// Clippy 1.94 tightened several lints; clean up incrementally rather than blocking CI.
#![allow(unstable_name_collisions)]
#![allow(
    deprecated,
    clippy::await_holding_lock,
    clippy::collapsible_if,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::empty_line_after_doc_comments,
    unused_imports,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::manual_repeat_n,
    clippy::manual_str_repeat,
    clippy::redundant_closure,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::unnecessary_map_or,
    clippy::useless_format,
    clippy::useless_vec
)]

// ── Crate-level imports used by internal modules ─────────────────────────────

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{body::Body, http::StatusCode, response::Response};
use serde::{Deserialize, Serialize};
use sqlx::{mysql::MySqlPoolOptions, query};
use uuid::Uuid;

use crate::bridge::{
    InMemoryTurnReflectionStateStore, NoopTurnObserverWorker, NoopTurnReflectionLessonWriter,
};

// ── Internal modules: HTTP handlers (crate-visible only) ─────────────────────

pub mod config_admin;
pub(crate) mod data_layer;
pub(crate) mod db_row;
pub mod messaging;

pub mod orchestration;
pub(crate) mod service_handlers;
pub mod skills;

// ── Backward-compatible module re-exports (moved into service_handlers/) ──────
//
// `agents` and `context` are still consumed as bare module names from
// `server/router_builder/*` via `use super::*;`. Keep them as crate-private
// re-exports until those call sites migrate to the canonical
// `service_handlers::{agents,context}` paths.
pub(crate) mod agents {
    pub use crate::service_handlers::agents::*;
}
pub(crate) mod context {
    pub use crate::service_handlers::context::*;
}

// ── self_model (the self_model/ directory) ──────────────────────────────────
#[path = "self_model/mod.rs"]
pub mod self_model;

pub use self_model::introspection;

// ── Internal modules: runtime storage helpers ────────────────────────────────

pub(crate) use data_layer::storage::{
    ensure_core_schema, insert_core_turn_event, insert_tool_turn_event, insert_turn_decision_audit,
    insert_turn_skill_selection, resolve_active_skill_versions, update_snapshot_llm_ids,
    update_turn_skill_selection_version,
};

// ── Public modules: runtime core ─────────────────────────────────────────────

mod app_state;
pub mod auto_invoke_handler;
pub mod bash_intent;
pub mod bridge;
pub mod capabilities;
pub(crate) mod capability_endpoint_pool;
pub mod capability_registry;
pub(crate) mod capacity_model;
pub mod deployment;
pub mod evaluation;
pub mod learning;
pub(crate) mod llm_provider_admission;
pub mod matrix_cloud_runtime;
pub mod memory_hooks;
pub mod observability;
pub mod pipeline;
pub use astra_plan as plan;
pub mod prompts;
pub mod provider;
pub mod server;
pub mod session_memory;
pub mod storage;
pub mod tool_registry;
pub mod turn;

#[doc(hidden)]
pub mod context_observability_bench {
    use serde_json::Value;

    pub fn augment_wire_trace(trace: &mut Value, messages: &[Value], tools: &[Value], debug: bool) {
        crate::turn::llm::context::augment_manifest_trace_with_wire_detail(
            trace,
            messages,
            tools,
            if debug {
                crate::turn::llm::context::WireTraceDetail::Debug
            } else {
                crate::turn::llm::context::WireTraceDetail::MetricsOnly
            },
        );
    }
}
pub use astra_sandbox as tool_sandbox;

// ── Re-exports: core primitives ──────────────────────────────────────────────

pub use astra_core::*;

// Re-export turn-core modules that CLI / edge paths reach into. Keeps astra-cli
// from needing a direct astra-turn-core dependency.
pub use astra_turn_core::recent_arg_hints;

/// Apply the `safety.trust_mode` field from a [`astra_config::RuntimeConfig`]
/// to the global safety guard state.
///
/// Call once at startup from any process that wants the runtime_config
/// TOML to be authoritative. Calling from user input / request args is a
/// bug — `TrustMode::Trusted` is an opt-in trust delegation from the
/// operator, not the LLM.
pub fn apply_safety_config_from_runtime_config(cfg: &astra_config::RuntimeConfig) {
    use astra_config::TrustModeSerde;
    use astra_turn_core::safety_middleware::{TrustMode, set_global_trust_mode};
    let mode = match cfg.safety.resolved_trust_mode() {
        TrustModeSerde::Strict => TrustMode::Strict,
        TrustModeSerde::Trusted => TrustMode::Trusted,
    };
    set_global_trust_mode(mode);
}

// ── Re-exports: service layer (via astra_services) ────────────────────────

pub use astra_services::{
    admin::{
        AdminAuditFilter, AdminAuditReader, AdminAuditRecord, AdminAuthorizer,
        AdminFeedbackStatsFilter, AdminFeedbackStatsReader, AdminFeedbackStatsRecord,
        AdminInitRecord, AdminInitializer, AdminTokenCreateRequestData, AdminTokenFilter,
        AdminTokenReader, AdminTokenRecord, AdminTokenWriter, AdminUserRoleManager,
        AdminUserRoleRecord, AdminUserRoleRequestData, AuthenticatedUser,
    },
    agents::{
        AgentCreateRequestData, AgentListItem, AgentListRecord, AgentRecord, AgentService,
        AgentUpdateRequestData, DatabaseAgentService, UnconfiguredAgentService,
    },
    auth::{
        AuthLoginRequestData, AuthPrincipal, AuthPrincipalOrigin, AuthRefreshRequestData,
        AuthRegisterRequestData, AuthService, AuthTokenRecord, AuthUserRecord,
        DatabaseAdminAuditReader, DatabaseAdminAuthorizer, DatabaseAdminFeedbackStatsReader,
        DatabaseAdminInitializer, DatabaseAdminTokenReader, DatabaseAdminTokenWriter,
        DatabaseAdminUserRoleManager, DatabaseAuthService, DatabaseSessionService,
        FernetTokenEncryptor, ReauthenticationProofRecord, ReauthenticationPurpose,
        ReauthenticationRequestData, SessionActivityRecord, SessionCreateRequestData,
        SessionListFilter, SessionListRecord, SessionRecord, SessionService,
        SessionUpdateRequestData,
    },
    branches::{BranchService, DatabaseBranchService, UnconfiguredBranchService},
    context::{
        ContextService, DatabaseContextService, SnapshotCreateRequestData, SnapshotListFilter,
        SnapshotListItem, SnapshotListRecord, SnapshotRecord, UnconfiguredContextService,
    },
    data_versioning::{
        DataVersioningService, DatabaseDataVersioningService, UnconfiguredDataVersioningService,
    },
    decisions::{
        DatabaseDecisionService, DecisionCreateRequestData, DecisionListFilter, DecisionListRecord,
        DecisionRecord, DecisionService, DecisionWithContextRecord, UnconfiguredDecisionService,
    },
    events::{
        DatabaseEventService, EventCreateRequestData, EventListFilter, EventListRecord,
        EventRecord, EventService, UnconfiguredEventService,
    },
    harness::{
        DatabaseHarnessService, HarnessCitationRecord, HarnessDecisionRequest, HarnessItemRecord,
        HarnessNodeCatalogRecord, HarnessRunRecord, HarnessService, HarnessSkillDraftRecord,
        HarnessSkillRuleRecord, HarnessTemplateRecord, SkillifyAgentCitation, SkillifyAgentDraft,
        SkillifyAgentExecutor, SkillifyAgentOutput, SkillifyAgentRequest, SkillifyAgentRule,
        SkillifyDraftRecord, SkillifyDraftRequest, SkillifyPublishRecord, SkillifyPublishRequest,
        SkillifyRunRequest, SkillifySourceFile, SkillifySourcePacket, UnconfiguredHarnessService,
    },
    jobs::{
        InMemoryJobService, JobRecord, JobService, JobSubmitRequestData, UnconfiguredJobService,
    },
    marketplace::{DatabaseMarketplaceService, MarketplaceService, UnconfiguredMarketplaceService},
    marketplace_stats::{
        DatabaseMarketplaceStatsService, MarketplaceStatsService, NoopMarketplaceStatsService,
    },
    mcp_registry::{
        DatabaseMcpRegistryService, McpRegisterRecord, McpRegisterRequestData, McpRegistryService,
        UnconfiguredMcpRegistryService,
    },
    models::{
        DatabaseModelService, ModelCreateRequestData, ModelListItem, ModelRecord, ModelService,
        ModelUpdateRequestData, PricingData, QuirksData, UnconfiguredModelService,
    },
    multi_agent::{
        DatabaseEdgeDispatchService, DatabaseEdgeRegistryService, DatabaseTaskLeaseService,
        EdgeDispatchIdentity, EdgeDispatchRow, EdgeDispatchService, EdgeRegistryService,
        TaskLeaseHoldCache, TaskLeaseService, UnconfiguredEdgeDispatchService,
        UnconfiguredEdgeRegistryService, UnconfiguredTaskLeaseService,
    },
    reflect::{DatabaseReflectService, ReflectReport, ReflectService, UnconfiguredReflectService},
    replay::{DatabaseReplayService, ReplayService, UnconfiguredReplayService},
    runs::{
        CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, RunLifecycleService,
        RunListRecord, RunStatusRecord,
    },
    sandbox::{DatabaseSandboxService, SandboxRecord, SandboxService, UnconfiguredSandboxService},
    session_audit::{
        AuditSessionListItem, AuditSessionListParams, AuditSessionListResponse, CrossSessionStats,
        CrossSessionStatsParams, CrossSessionToolAnalytics, DatabaseSessionAuditService,
        ModelUsageBrief, SessionAuditService, ToolUsageBrief, UnconfiguredSessionAuditService,
    },
    session_journal,
    skill_config::{
        DatabaseSkillConfigService, SkillConfigService, UnconfiguredSkillConfigService,
    },
    skills::{DatabaseSkillService, SkillRecord, SkillService, UnconfiguredSkillService},
    task_orchestrator::{
        MatrixOneTaskService, TaskCreateRequest, TaskRecord, TaskService, TaskStatus,
        UnconfiguredTaskService,
    },
    triggers::{DatabaseTriggerService, TriggerRecord, TriggerService, UnconfiguredTriggerService},
    workflows::{
        UnconfiguredWorkflowService, WorkflowDefRecord, WorkflowListItem, WorkflowRunRecord,
        WorkflowService,
    },
};

pub(crate) use astra_services::runs::UnconfiguredRunLifecycleService;

// ── Re-exports: runtime app state ────────────────────────────────────────────

pub use app_state::{
    AppState, DatabaseHealth, HealthChecker, MatrixOneHealthChecker, MemoriaForwarder,
    MemoriaHealth, NoopMemoriaForwarder, ReqwestMemoriaForwarder, ServiceInfo,
};

// ── Re-exports: bridge ───────────────────────────────────────────────────────

pub use bridge::{
    CooldownReason, DatabaseTurnObserverWorker, DatabaseTurnReflectionLessonWriter,
    RateLimitAction, RateLimitCooldown, RateLimitMetrics, RateLimitState,
    side_effects::{PERSIST_FAIL_COUNT, PERSIST_OK_COUNT},
};

// ── Re-exports: evaluation & introspection ───────────────────────────────────

pub use evaluation::{DatabaseEvaluationService, EvaluationService, UnconfiguredEvaluationService};
pub use introspection::{
    DatabaseIntrospectionService, IntrospectionService, UnconfiguredIntrospectionService,
};

// ── Re-exports: server ───────────────────────────────────────────────────────

pub use server::build_test_router;
pub use server::delegation::engine::{
    CheckpointGate, DefaultQualityGate, DelegationEngine, DelegationTracker, GateVerdict,
    QualityThresholds, VerificationGate,
};
pub use server::run::engine::RunEngine;
pub use server::run::lifecycle::AgenticRunLifecycleService;
pub use server::{build_app, build_server_state, serve};

// ── Re-exports: orchestration ────────────────────────────────────────────────

pub use orchestration::{AgentHistoryRecord, AgentRegistry};

// ── Re-exports: turn engine ──────────────────────────────────────────────────

pub(crate) use astra_turn_core::contracts::TurnReflectionLessonRequest;

pub use astra_turn_core::contracts::{
    TurnAuxiliaryEventRecord, TurnAuxiliaryEventWriter, TurnCoreEventRecord, TurnCoreEventWriter,
    TurnCorePersistOutcome, TurnCorePersistPlan, TurnDecisionAuditRecord, TurnHookDbPersistPlan,
    TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker, TurnReflectionLessonRecord,
    TurnReflectionLessonWriter, TurnReflectionMark, TurnReflectionStateStore,
    TurnSessionActivityWriter, TurnSkillSelectionRecord, TurnToolEventPersistPlan,
    TurnToolEventRecord, TurnToolEventWriter,
};

pub use turn::services::{
    DatabaseTraceEventWriter, DatabaseTurnAuxiliaryEventWriter, DatabaseTurnCoreEventWriter,
    DatabaseTurnHookDbWriter, DatabaseTurnSessionActivityWriter, DatabaseTurnToolEventWriter,
};

pub use astra_turn_core::{
    action_compensation::{
        ActionCategory, ActionCompensationProfile, CompensationKind, compensation_prompt_note,
        explicit_approval_reason, tool_action_profile, tool_action_profile_value,
        tool_requires_explicit_approval,
    },
    activity::SessionActivityUpdatePlan,
    cache::SessionCache,
    cloud_attachments::{
        AttachmentBuilder, FileAttachment, PlanAttachment, PostCompactAttachments, SkillAttachment,
    },
    cloud_cache_diagnostics::{
        CacheBreakCause, CacheBreakDetector, CacheBreakEvent, CacheFingerprint, diff_fingerprints,
    },
    cloud_session_memory_extract::{
        SESSION_MEMORY_TEMPLATE, SessionMemoryState, build_extraction_prompt, extract_section,
        should_extract as should_extract_session_memory,
    },
    cloud_summary::{SummaryLlmClient, SummaryResponse},
    complete::build_turn_complete_event,
    execution_state::normalize_execution_state,
    explain::build_explain_event,
    history::{
        RecoveredEventRow, append_recovered_events, find_tool_call_safe_split,
        merge_tool_results_into_history,
    },
    hook_plans::{SnapshotLinkPlan, build_snapshot_link_plan},
    observer::{build_observer_messages, should_run_observer},
    persist::{
        LlmResponsePersistPlan, PersistEventPayload, build_llm_response_persist_plan,
        build_tool_call_event_payload, build_tool_result_event_payload,
    },
    response_guard::{is_prompt_leaked, is_repetition_loop},
    routing::build_skipped_routing_metadata,
    stall::{
        DIVERGENCE_CORRECTION, DivergenceStatus, SERVER_STALL_WINDOW, canonical_tool_args,
        detect_divergence, detect_server_stall, record_server_tool_signatures,
        server_tool_call_signature,
    },
    state::{new_session_entry, normalize_bridge_cache_entry, resolve_turn_identifiers},
    stream_events::{
        build_approval_required_event, build_edge_tool_call_event, build_firewall_warning_event,
        build_runtime_error_event, build_stream_error_event, build_tool_request_event,
    },
    tail_persist::build_turn_hook_args,
    tool_surface_subset::{plan_tool_subset_for_result_turn, resolve_preferred_tool_status},
    turn_metrics::build_tool_result_quality_event_payload,
    view::{
        RetrievalPlan, build_recent_retrieval_tail, compose_retrieval_view,
        extract_latest_user_query, plan_retrieval_inputs,
    },
};
pub use turn::cloud::{
    analytics::{
        CompactionEvent, CompactionEventType, MICRO_COMPACT_STUB, MessageRange,
        PartialCompactRequest, PartialCompactResult, TurnCountCompactConfig, TurnCountTrigger,
        apply_micro_compact, compact_partial, evaluate_turn_count_trigger,
    },
    compaction::{CompactBoundary, CompactCircuitBreaker, CompactResult, CompactTrigger},
    memoria_compact::{
        HttpMemoriaPort, MemoriaCompactConfig, MemoriaCompactParams, MemoriaMemory, MemoriaPort,
        claude_code_session_memory_path, compact_with_memoria, memoria_compact_retrieve_query,
        sanitize_path_for_claude_projects,
    },
};

pub use astra_config::runtime_config;
pub use astra_config::user_profile;
pub use astra_text_utils::semantic_dedup;
pub use astra_text_utils::text_tokenize;
pub use astra_turn_core::cloud_session_facts::update_from_journal_event;
pub use astra_turn_types::session_facts::{ErrorFact, FileEntry, SessionFacts, ToolFact};
pub use matrix_cloud_runtime::{MatrixCloudRuntime, matrix_settings_from_env};
