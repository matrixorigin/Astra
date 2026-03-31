// ── Crate-level imports used by internal modules ─────────────────────────────

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use async_stream::stream;
use async_trait::async_trait;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use futures_util::StreamExt;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{mysql::MySqlPoolOptions, query};
use uuid::Uuid;

use crate::bridge::{
    HttpChatTurnBridge, InMemoryTurnReflectionStateStore, NoopTurnObserverWorker,
    NoopTurnReflectionLessonWriter, UnavailableChatTurnBridge,
};

// ── Internal modules: HTTP handlers (crate-visible only) ─────────────────────

pub(crate) mod agents;
pub mod branches;
pub(crate) mod context;
pub mod data_versioning;
pub(crate) mod decisions;
pub(crate) mod events;
pub mod jobs;
pub mod marketplace;
pub(crate) mod models;
pub mod replay;
pub mod sandbox;
pub mod semantic_dedup;
pub mod skill_config;
pub mod skills;
pub mod streaming;
pub mod text_tokenize;
pub mod tool_sandbox;
pub mod triggers;
pub(crate) mod workflows;

// ── Internal modules: runtime storage helpers ────────────────────────────────

pub(crate) mod storage;

pub(crate) use storage::{
    ensure_core_schema, insert_core_turn_event, insert_tool_turn_event, insert_turn_decision_audit,
    insert_turn_implicit_feedback, insert_turn_skill_selection, resolve_active_skill_versions,
    update_snapshot_llm_ids, update_turn_skill_selection_version,
};

// ── Public modules: runtime core ─────────────────────────────────────────────

mod app_state;
pub mod bridge;
pub mod evaluation;
pub mod introspection;
pub mod matrix_cloud_runtime;
pub mod pipeline;
pub mod plan_decompose;
pub mod prompts;
pub mod server;
pub mod sync_adapters;
pub mod tool_registry;
pub mod tool_selector;
pub mod turn;

// ── Re-exports: core primitives ──────────────────────────────────────────────

pub use mo_agent_core::*;

// ── Re-exports: service layer (via mo_agent_services) ────────────────────────

pub use mo_agent_services::{
    admin::{
        AdminAuditFilter, AdminAuditReader, AdminAuditRecord, AdminAuthorizer,
        AdminFeedbackStatsFilter, AdminFeedbackStatsReader, AdminFeedbackStatsRecord,
        AdminInitRecord, AdminInitializer, AdminTokenCreateRequestData, AdminTokenFilter,
        AdminTokenReader, AdminTokenRecord, AdminTokenWriter, AdminUserRoleManager,
        AdminUserRoleRecord, AdminUserRoleRequestData, AuthenticatedUser,
    },
    agents::{
        AgentCreateRequestData, AgentListRecord, AgentRecord, AgentService, AgentUpdateRequestData,
        DatabaseAgentService, UnconfiguredAgentService,
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
        SnapshotListRecord, SnapshotRecord, UnconfiguredContextService,
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
    models::{
        DatabaseModelService, ModelCreateRequestData, ModelRecord, ModelService,
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
        DatabaseWorkflowService, UnconfiguredWorkflowService, WorkflowDefRecord, WorkflowRunRecord,
        WorkflowService,
    },
};

pub(crate) use mo_agent_services::runs::{
    RunMutationRecord, UnconfiguredRunLifecycleService, transform_run_event_for_client,
};

// ── Re-exports: runtime app state ────────────────────────────────────────────

pub use app_state::{
    AppState, HealthChecker, MatrixOneHealthChecker, MemoriaForwarder, NoopMemoriaForwarder,
    ReqwestMemoriaForwarder, ServiceInfo,
};

// ── Re-exports: bridge ───────────────────────────────────────────────────────

pub use bridge::{
    ChatTurnBridge, DatabaseTurnObserverWorker, DatabaseTurnReflectionLessonWriter,
    side_effects::{PERSIST_FAIL_COUNT, PERSIST_OK_COUNT},
};

// ── Re-exports: evaluation & introspection ───────────────────────────────────

pub use evaluation::{DatabaseEvaluationService, EvaluationService, UnconfiguredEvaluationService};
pub use introspection::{
    DatabaseIntrospectionService, IntrospectionService, UnconfiguredIntrospectionService,
};

// ── Re-exports: server ───────────────────────────────────────────────────────

pub use server::{build_app, serve};

// ── Re-exports: turn engine ──────────────────────────────────────────────────

pub(crate) use turn::contracts::TurnReflectionLessonRequest;

pub use turn::contracts::{
    TurnAuxiliaryEventRecord, TurnAuxiliaryEventWriter, TurnCoreEventRecord, TurnCoreEventWriter,
    TurnCorePersistOutcome, TurnCorePersistPlan, TurnDecisionAuditRecord, TurnHookDbPersistPlan,
    TurnHookDbWriter, TurnImplicitFeedbackRecord, TurnLearningOutcome, TurnLearningWriter,
    TurnObserverRequest, TurnObserverWorker, TurnReflectionLessonRecord,
    TurnReflectionLessonWriter, TurnReflectionMark, TurnReflectionStateStore,
    TurnSessionActivityWriter, TurnSkillSelectionRecord, TurnToolEventPersistPlan,
    TurnToolEventRecord, TurnToolEventWriter,
};

pub use turn::services::{
    DatabaseTurnAuxiliaryEventWriter, DatabaseTurnCoreEventWriter, DatabaseTurnHookDbWriter,
    DatabaseTurnSessionActivityWriter, DatabaseTurnToolEventWriter,
};

pub use turn::{
    activity::{SessionActivityUpdatePlan, build_session_activity_update_plan},
    cache::SessionCache,
    cloud::{
        compaction::compact_tiered,
        history::compact_cloud_loop_history,
        iteration::{CloudLoopIterationPlan, plan_cloud_loop_iteration},
        prefilter::{CloudSkillCandidatePlan, plan_cloud_skill_candidates},
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
