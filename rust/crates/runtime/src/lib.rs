// Clippy 1.94 tightened several lints; clean up incrementally rather than blocking CI.
#![allow(
    deprecated,
    clippy::await_holding_lock,
    clippy::collapsible_if,
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

pub(crate) mod admin_config_handlers;
pub(crate) mod agents;
pub mod branches;
pub(crate) mod context;
pub mod data_versioning;
pub(crate) mod decisions;
pub(crate) mod events;
pub mod jobs;
pub mod marketplace;
pub mod messaging;
pub(crate) mod models;
pub mod orchestration;
pub mod replay;
pub mod sandbox;
pub mod skill_config;
pub mod skills;
pub mod triggers;
pub(crate) mod workflows;

// ── Internal modules: runtime storage helpers ────────────────────────────────

pub(crate) mod storage;

pub(crate) use storage::{
    ensure_core_schema, insert_core_turn_event, insert_tool_turn_event, insert_turn_decision_audit,
    insert_turn_implicit_feedback, insert_turn_skill_selection, insert_turn_skill_selector_metric,
    resolve_active_skill_versions, trim_turn_skill_selector_metrics_window,
    update_snapshot_llm_ids, update_turn_skill_selection_version,
};

// ── Public modules: runtime core ─────────────────────────────────────────────

mod app_state;
pub mod bash_intent;
pub mod bridge;
pub mod evaluation;
pub mod evolution;
pub mod guardrail_tuning;
pub mod introspection;
pub mod matrix_cloud_runtime;
pub mod memoria_insights;
pub mod observability_integration;
pub mod pipeline;
pub use astra_plan as plan;
pub use astra_plan as plan_decompose;
pub use astra_sandbox as tool_sandbox;
pub use astra_sync_adapters as sync_adapters;
pub mod liquid {
    pub use astra_evolution::reflection;
}
pub mod prompts;
pub mod self_model;
pub mod server;
pub mod tool_registry;
pub mod tool_selector;
pub mod turn;

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
        AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
        AuthTokenRecord, AuthUserRecord, DatabaseAdminAuditReader, DatabaseAdminAuthorizer,
        DatabaseAdminFeedbackStatsReader, DatabaseAdminInitializer, DatabaseAdminTokenReader,
        DatabaseAdminTokenWriter, DatabaseAdminUserRoleManager, DatabaseAuthService,
        DatabaseSessionService, FernetTokenEncryptor, SessionActivityRecord,
        SessionCreateRequestData, SessionListFilter, SessionListRecord, SessionRecord,
        SessionService, SessionUpdateRequestData,
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
    jobs::{
        InMemoryJobService, JobRecord, JobService, JobSubmitRequestData, UnconfiguredJobService,
    },
    learning::{
        DatabaseLearningFeedbackService, LearningFeedbackRecord, LearningFeedbackRequestData,
        LearningFeedbackService, UnconfiguredLearningFeedbackService,
    },
    marketplace::{DatabaseMarketplaceService, MarketplaceService, UnconfiguredMarketplaceService},
    marketplace_stats::{
        DatabaseMarketplaceStatsService, MarketplaceStatsService, NoopMarketplaceStatsService,
    },
    models::{
        DatabaseModelService, ModelCreateRequestData, ModelListItem, ModelRecord, ModelService,
        ModelUpdateRequestData, PricingData, QuirksData, UnconfiguredModelService,
    },
    multi_agent::{
        DatabaseEdgeRegistryService, DatabaseTaskLeaseService, EdgeRegistryService,
        TaskLeaseHoldCache, TaskLeaseService, UnconfiguredEdgeRegistryService,
        UnconfiguredTaskLeaseService,
    },
    reflect::{
        DatabaseReflectService, Diagnosis, ErrorClass, ReflectReport, ReflectService,
        UnconfiguredReflectService,
    },
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
        DatabaseWorkflowService, UnconfiguredWorkflowService, WorkflowDefRecord, WorkflowListItem,
        WorkflowRunRecord, WorkflowService,
    },
};

pub(crate) use astra_services::runs::UnconfiguredRunLifecycleService;

// ── Re-exports: runtime app state ────────────────────────────────────────────

pub use app_state::{
    AppState, HealthChecker, MatrixOneHealthChecker, MemoriaForwarder, NoopMemoriaForwarder,
    ReqwestMemoriaForwarder, ServiceInfo,
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

pub use server::delegation_engine::{
    CheckpointGate, DefaultQualityGate, DelegationEngine, DelegationTracker, GateVerdict,
    QualityThresholds, VerificationGate,
};
pub use server::run_engine::RunEngine;
pub use server::run_lifecycle::AgenticRunLifecycleService;
pub use server::{build_app, build_server_state, serve};

// ── Re-exports: orchestration ────────────────────────────────────────────────

pub use orchestration::{AgentHistoryRecord, AgentRegistry};

// ── Re-exports: turn engine ──────────────────────────────────────────────────

pub(crate) use astra_turn_core::contracts::TurnReflectionLessonRequest;

pub use astra_turn_core::contracts::{
    TurnAuxiliaryEventRecord, TurnAuxiliaryEventWriter, TurnCoreEventRecord, TurnCoreEventWriter,
    TurnCorePersistOutcome, TurnCorePersistPlan, TurnDecisionAuditRecord, TurnHookDbPersistPlan,
    TurnHookDbWriter, TurnImplicitFeedbackRecord, TurnLearningOutcome, TurnLearningWriter,
    TurnObserverRequest, TurnObserverWorker, TurnReflectionLessonRecord,
    TurnReflectionLessonWriter, TurnReflectionMark, TurnReflectionStateStore,
    TurnSessionActivityWriter, TurnSkillSelectionRecord, TurnSkillSelectorMetricRecord,
    TurnToolEventPersistPlan, TurnToolEventRecord, TurnToolEventWriter,
};

pub use turn::services::{
    DatabaseTurnAuxiliaryEventWriter, DatabaseTurnCoreEventWriter, DatabaseTurnHookDbWriter,
    DatabaseTurnSessionActivityWriter, DatabaseTurnToolEventWriter,
};

pub use astra_turn_core::{
    action_compensation::{
        ActionCategory, ActionCompensationProfile, CompensationKind, compensation_prompt_note,
        explicit_approval_reason, tool_action_profile, tool_action_profile_value,
        tool_requires_explicit_approval,
    },
    activity::{SessionActivityUpdatePlan, build_session_activity_update_plan},
    cache::SessionCache,
    cloud_attachments::{
        AttachmentBuilder, FileAttachment, PlanAttachment, PostCompactAttachments, SkillAttachment,
    },
    cloud_cache_diagnostics::{
        CacheBreakCause, CacheBreakDetector, CacheBreakEvent, CacheFingerprint, diff_fingerprints,
    },
    cloud_session_memory_extract::{
        SESSION_MEMORY_TEMPLATE, SessionMemoryExtractConfig, SessionMemoryState,
        build_extraction_prompt, build_learnings_extraction_prompt, extract_learnings_for_backflow,
        extract_section, parse_learnings_response, should_extract as should_extract_session_memory,
        should_extract_with_error_trigger, truncate_for_prompt, write_session_memory_file,
    },
    cloud_summary::{HttpSummaryClient, LlmConnParams, SummaryLlmClient, SummaryResponse},
    complete::build_turn_complete_event,
    counter::count_persisted_turn_events,
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
    quality::build_tool_result_quality_event_payload,
    response_guard::{is_prompt_leaked, is_repetition_loop},
    retrieval::{
        RETRIEVAL_BUDGET_CHARS, enhanced_extraction, format_retrieved_events, rule_based_extraction,
    },
    routing::{
        MAX_TOOL_ROUNDS, build_routing_metadata, build_skipped_routing_metadata, detect_correction,
    },
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
    task::classify_task,
    tool_args_repair::try_repair_tool_args,
    tool_selection::{plan_tool_subset_for_result_turn, resolve_preferred_tool_status},
    view::{
        RetrievalPlan, build_recent_retrieval_tail, compose_retrieval_view,
        extract_latest_user_query, plan_retrieval_inputs,
    },
};
pub use turn::{
    cloud::{
        analytics::{
            CompactionEvent, CompactionEventType, MICRO_COMPACT_STUB, MessageRange,
            PartialCompactRequest, PartialCompactResult, TurnCountCompactConfig, TurnCountTrigger,
            apply_micro_compact, compact_partial, evaluate_turn_count_trigger,
        },
        compaction::{
            CompactBoundary, CompactCircuitBreaker, CompactResult, CompactTrigger, compact_tiered,
            compact_tiered_with_result, compact_with_summary,
        },
        memoria_compact::{
            HttpMemoriaClient, MemoriaClient, MemoriaCompactConfig, MemoriaCompactParams,
            MemoriaMemory, SessionMemoryFileCombine, claude_code_session_memory_path,
            compact_with_memoria, compact_with_memoria_sync, memoria_compact_retrieve_query,
            read_session_memory_file, resolve_resume_session_memory_file,
            resolve_session_memory_file_options, sanitize_path_for_claude_projects,
        },
    },
    implicit_feedback::{
        ImplicitSignal, detect_implicit_feedback_signal, implicit_feedback_rating,
    },
};

pub use astra_config::runtime_config;
pub use astra_config::user_profile;
pub use astra_learning::auto_tuning;
pub use astra_text_utils::semantic_dedup;
pub use astra_text_utils::text_tokenize;
pub use astra_turn_core::cloud_session_facts::update_from_journal_event;
pub use astra_turn_types::session_facts::{ErrorFact, FileEntry, PlanFact, SessionFacts, ToolFact};
pub use matrix_cloud_runtime::{
    MatrixCloudRuntime, build_sync_orchestrator_with_adapters, matrix_settings_from_env,
};
