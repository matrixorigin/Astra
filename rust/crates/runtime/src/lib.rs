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

pub(crate) mod agents;
pub mod branches;
pub(crate) mod context;
pub mod data_versioning;
pub(crate) mod decisions;
pub(crate) mod events;
pub mod jobs;
pub(crate) mod lock_ext;
pub mod marketplace;
pub mod messaging;
pub(crate) mod models;
pub mod orchestration;
pub mod replay;
pub mod sandbox;
pub mod semantic_dedup;
pub mod skill_config;
pub mod skills;
pub mod str_preview;
pub mod streaming;
pub mod text_tokenize;
pub mod tool_sandbox;
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
pub mod auto_tuning;
pub mod bridge;
pub mod evaluation;
pub mod evolution;
pub mod execution_profile;
pub mod guardrail_tuning;
pub mod introspection;
pub mod liquid;
pub mod matrix_cloud_runtime;
pub mod memoria_insights;
pub mod observability_integration;
pub mod output_style;
pub mod pipeline;
pub mod plan;
pub mod plan_decompose;
pub mod prompts;
pub mod runtime_config;
pub mod self_model;
pub mod server;
pub mod sync_adapters;
pub mod tool_registry;
pub mod tool_selector;
pub mod turn;
pub mod user_profile;

// ── Re-exports: core primitives ──────────────────────────────────────────────

pub use astra_core::*;

// Re-export turn-core modules that CLI / edge paths reach into. Keeps astra-cli
// from needing a direct astra-turn-core dependency.
pub use astra_turn_core::recent_arg_hints;

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
    streaming::{StreamingService, UnconfiguredStreamingService},
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

pub(crate) use turn::contracts::TurnReflectionLessonRequest;

pub use turn::contracts::{
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

pub use turn::{
    action_compensation::{
        ActionCategory, ActionCompensationProfile, CompensationKind, compensation_prompt_note,
        explicit_approval_reason, tool_action_profile, tool_action_profile_value,
        tool_requires_explicit_approval,
    },
    activity::{SessionActivityUpdatePlan, build_session_activity_update_plan},
    cache::SessionCache,
    cloud::{
        analytics::{
            CompactionEvent, CompactionEventType, MICRO_COMPACT_STUB, MessageRange,
            PartialCompactRequest, PartialCompactResult, TimeBasedCompactConfig, TimeBasedTrigger,
            TurnCountCompactConfig, TurnCountTrigger, apply_micro_compact, compact_partial,
            evaluate_time_based_trigger, evaluate_turn_count_trigger, run_micro_compact,
        },
        attachments::{
            AttachmentBuilder, FileAttachment, PlanAttachment, PostCompactAttachments,
            SkillAttachment,
        },
        cache_diagnostics::{
            CacheBreakCause, CacheBreakDetector, CacheBreakEvent, CacheFingerprint,
            diff_fingerprints,
        },
        compaction::{
            CompactBoundary, CompactCircuitBreaker, CompactResult, CompactTrigger, compact_tiered,
            compact_tiered_with_result, compact_with_summary,
        },
        history::compact_cloud_loop_history,
        iteration::{CloudLoopIterationPlan, plan_cloud_loop_iteration},
        memoria_compact::{
            HttpMemoriaClient, MemoriaClient, MemoriaCompactConfig, MemoriaCompactParams,
            MemoriaMemory, SessionMemoryFileCombine, claude_code_session_memory_path,
            compact_with_memoria, compact_with_memoria_sync, memoria_compact_retrieve_query,
            read_session_memory_file, resolve_resume_session_memory_file,
            resolve_session_memory_file_options, sanitize_path_for_claude_projects,
        },
        prefilter::{CloudSkillCandidatePlan, plan_cloud_skill_candidates},
        session_facts::{ErrorFact, FileEntry, PlanFact, SessionFacts, ToolFact},
        session_memory_extract::{
            SESSION_MEMORY_TEMPLATE, SessionMemoryExtractConfig, SessionMemoryState,
            build_extraction_prompt, build_learnings_extraction_prompt,
            extract_learnings_for_backflow, extract_section, parse_learnings_response,
            should_extract as should_extract_session_memory, should_extract_with_error_trigger,
            truncate_for_prompt, write_session_memory_file,
        },
        summary::{HttpSummaryClient, LlmConnParams, SummaryLlmClient, SummaryResponse},
    },
    complete::build_turn_complete_event,
    counter::count_persisted_turn_events,
    execution_state::normalize_execution_state,
    explain::build_explain_event,
    firewall::build_firewall_verification_plan,
    history::{
        RecoveredEventRow, append_recovered_events, find_tool_call_safe_split,
        merge_tool_results_into_history,
    },
    history_apply::apply_turn_inputs_to_history,
    hook_plans::{SnapshotLinkPlan, build_snapshot_link_plan},
    implicit_feedback::{
        ImplicitSignal, detect_implicit_feedback_signal, implicit_feedback_rating,
    },
    observer::{build_observer_messages, should_run_observer},
    persist::{
        LlmResponsePersistPlan, PersistEventPayload, build_llm_response_persist_plan,
        build_tool_call_event_payload, build_tool_result_event_payload,
    },
    persist_inputs::{build_routing_decision_event_payload, collect_skill_version_names},
    quality::build_tool_result_quality_event_payload,
    refresh::{extract_first_user_query, plan_memory_refresh},
    response_guard::{is_prompt_leaked, is_repetition_loop},
    retrieval::{
        RETRIEVAL_BUDGET_CHARS, enhanced_extraction, format_retrieved_events, rule_based_extraction,
    },
    routing::{
        MAX_TOOL_ROUNDS, build_routing_metadata, build_skipped_routing_metadata, detect_correction,
    },
    routing_metrics::{RoutingMetricsPlan, build_routing_metrics_plan},
    session_cache::apply_turn_to_session_entry,
    snapshot::{build_session_history_snapshot, should_persist_session_history_snapshot},
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
    tail_persist::{
        build_cached_assistant_message, build_persist_thread_args, build_turn_hook_args,
    },
    task::classify_task,
    tool_args_repair::try_repair_tool_args,
    tool_selection::{plan_tool_subset_for_result_turn, resolve_preferred_tool_status},
    unconsumed::{build_unconsumed_tool_messages, latest_assistant_tool_call_ids},
    view::{
        RetrievalPlan, build_recent_retrieval_tail, compose_retrieval_view,
        extract_latest_user_query, plan_retrieval_inputs,
    },
};

pub use matrix_cloud_runtime::{
    MatrixCloudRuntime, build_sync_orchestrator_with_adapters, matrix_settings_from_env,
};
