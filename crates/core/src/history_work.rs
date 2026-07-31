//! Low-cardinality Phase-0 instrumentation for redundant history work.
//!
//! The provider must process the final prompt bytes. These counters isolate
//! additional local O(history) work: deep clones, hashing, serialization,
//! database rows, queued resident bytes, and fixed-count admission units.
//! Message contents and session/request IDs are never recorded.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;

const TRACE_ENV: &str = "ASTRA_HISTORY_WORK_TRACE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum HistoryWorkSite {
    AgenticRequestSnapshot,
    CliPostCommitSnapshot,
    CliSettlementRollbackSnapshot,
    CliPrimaryCommitWorkerHistoryQueue,
    CliPrimaryCommitWorkerFinalMessagesQueue,
    CliDebugCheckpointRead,
    CliDebugCheckpointDeserialization,
    CliDebugCheckpointHistoryClone,
    CliDebugHistoryDeltaClone,
    CliDebugHistoryDeltaComparison,
    CliDebugDumpPayloadClone,
    CliDebugDumpSerialization,
    CliContextDumpJournalHistoryMaterialization,
    CliContextDumpSerialization,
    CliCompletionProxyMessageClone,
    CliDisplayHistoryProjectionClone,
    CliForkChildHistoryMaterialization,
    CliForkFrozenToolSchemaClone,
    CliForkPrefixSerialization,
    CliForkToolSchemaSerialization,
    CliHistoryEditMemoryMaterialization,
    CliHistoryEditRollbackSnapshot,
    CliManualCompactionHistoryMaterialization,
    CliManualCompactionRetainedHistoryClone,
    CliManualCompactionSwapProjection,
    CliMemoryInferenceRequestClone,
    CliOneShotContinuationClone,
    CliPlanBackgroundHistoryClone,
    CliPlanBackgroundHistoryQueue,
    CliPostCommitQueue,
    CliPromptContinuationSanitization,
    CliPromptHistoryMaterialization,
    CliPromptNormalizationClone,
    CliPromptPayloadClone,
    CliRecoveryCheckpointHistoryMaterialization,
    CliRecoveryCslBackupRead,
    CliRecoveryCslHistoryMaterialization,
    CliRecoveryCslLogRead,
    CliRecoveryCslLogDeserialization,
    CliRecoveryCslSnapshotClone,
    CliRecoveryCslSnapshotSerialization,
    CliResumeCanonicalHistoryClone,
    CliResumeHistoryMaterialization,
    CliJournalHistoryHydration,
    CliJournalDigestMaterialization,
    CliJournalDigestSerialization,
    CliSessionMemoryShutdownMaterialization,
    CliSessionRestoreHydration,
    CliSlashForkCanonicalHistoryClone,
    CliSlashForkJournalHistoryClone,
    CliSlashForkRollbackSnapshot,
    CliSubrunPromptPayloadClone,
    CliTaskBackgroundHistoryClone,
    CliTaskBackgroundHistoryQueue,
    CliTurnRetryHistoryClone,
    CliTurnUserInputProjection,
    CslMaterializedStateClone,
    CslPersistInputClone,
    CslStateInstallClone,
    CslSnapshotClone,
    CslMessageHash,
    CslHashIndexClone,
    CslFileRead,
    CslFileAppendSerialization,
    CslFileRewriteSerialization,
    CslDatabaseRead,
    CslDatabaseSerialization,
    CslDatabaseRows,
    PromptDeltaHash,
    PromptDeltaRead,
    PromptDeltaRows,
    PromptDeltaUnchangedPrefix,
    RunAdmission,
    FinalizationCheckpointClone,
    FinalizationRecoveryComparison,
    PipelineHeavyCheckpointClone,
    PipelineCheckpointSerialization,
    PipelineCheckpointRead,
    PipelineCompositeIndexSerialization,
    PipelineCompositeIndexRead,
    PipelineRecoveryClone,
    PipelineRecoverySerialization,
    PipelineRecoveryHash,
    PipelineEventJournalRead,
    ContextBinding,
    ContextOptimization,
    ContextSerialization,
    RuntimeContextMaterialization,
    TurnTraceHistoryClone,
    PromptCacheHistoryScan,
    HistoryBudgetEstimationSerialization,
    LlmWireTraceClone,
    LlmWireTraceHash,
    ProviderWireAssembly,
    ProviderBodySerialization,
    ProviderRetryRetention,
    LlmCaptureArtifactClone,
    LlmCaptureArtifactSerialization,
    BridgePipelineInputMaterialization,
    BridgeJournalReplayClone,
    BridgeJournalReplaySerialization,
    BridgeRequestCaptureClone,
    BridgeDisconnectCaptureClone,
    BridgeCompactionFixedContextClone,
    ServerCslPersistClone,
    ServerObserverQueue,
    ServerCompactionFixedContextClone,
    ServerToolPolicySchemaClone,
    ServerToolAdmissionSnapshotClone,
    ServerToolSchemaEstimationSerialization,
    DelegationContextClone,
    DelegationRetryContextClone,
    DelegationParentMessagesClone,
    ObservabilityRollbackSnapshotClone,
    ObservabilityRollbackRestoreClone,
    MemoryExtractionHistoryClone,
    MemoryExtractionPromptSanitization,
    MemoryExtractionPayloadSerialization,
    MemoryExtractionQueue,
    ForkPrefixMaterialization,
    ForkPrefixHash,
    ForkToolSchemaHash,
    ForkPrefixReconstruction,
    ServerForkToolSchemaClone,
    ServerForkPrefixSerialization,
    ServerRequestCaptureClone,
    ServerContextTraceClone,
    ToolSchemaCacheStabilizationClone,
    ToolSchemaWireSortSerialization,
    SessionRestoreHydration,
    SessionRestoreTranscriptHydration,
    ResumeHintHistoryClone,
    TailPersistHistoryClone,
    CompactionHistoryClone,
    CompactionHistorySerialization,
    RequestDumpClone,
    RequestDumpSerialization,
    CloudHistoryGroupingClone,
    CloudSummaryPromptClone,
    CloudSummarySerialization,
    SyncOutboxPayloadClone,
    SyncOutboxPayloadHash,
    SyncOutboxJournalDeltaClone,
    SyncOutboxStateClone,
    SyncOutboxDeliveryClone,
    SyncOutboxDeliverySerialization,
    SyncOutboxQueueRead,
    SyncOutboxQueueRewrite,
    SessionJournalHistorySerialization,
    SessionJournalFullRead,
    SessionJournalTailRead,
    SessionJournalDigestRead,
    SessionJournalAppendDeltaRead,
    EventIngestionJournalHash,
    EventIngestionHistoryClone,
    EventIngestionQueue,
}

impl HistoryWorkSite {
    const COUNT: usize = 156;

    pub const ALL: [Self; Self::COUNT] = [
        Self::AgenticRequestSnapshot,
        Self::CliPostCommitSnapshot,
        Self::CliSettlementRollbackSnapshot,
        Self::CliPrimaryCommitWorkerHistoryQueue,
        Self::CliPrimaryCommitWorkerFinalMessagesQueue,
        Self::CliDebugCheckpointRead,
        Self::CliDebugCheckpointDeserialization,
        Self::CliDebugCheckpointHistoryClone,
        Self::CliDebugHistoryDeltaClone,
        Self::CliDebugHistoryDeltaComparison,
        Self::CliDebugDumpPayloadClone,
        Self::CliDebugDumpSerialization,
        Self::CliContextDumpJournalHistoryMaterialization,
        Self::CliContextDumpSerialization,
        Self::CliCompletionProxyMessageClone,
        Self::CliDisplayHistoryProjectionClone,
        Self::CliForkChildHistoryMaterialization,
        Self::CliForkFrozenToolSchemaClone,
        Self::CliForkPrefixSerialization,
        Self::CliForkToolSchemaSerialization,
        Self::CliHistoryEditMemoryMaterialization,
        Self::CliHistoryEditRollbackSnapshot,
        Self::CliManualCompactionHistoryMaterialization,
        Self::CliManualCompactionRetainedHistoryClone,
        Self::CliManualCompactionSwapProjection,
        Self::CliMemoryInferenceRequestClone,
        Self::CliOneShotContinuationClone,
        Self::CliPlanBackgroundHistoryClone,
        Self::CliPlanBackgroundHistoryQueue,
        Self::CliPostCommitQueue,
        Self::CliPromptContinuationSanitization,
        Self::CliPromptHistoryMaterialization,
        Self::CliPromptNormalizationClone,
        Self::CliPromptPayloadClone,
        Self::CliRecoveryCheckpointHistoryMaterialization,
        Self::CliRecoveryCslBackupRead,
        Self::CliRecoveryCslHistoryMaterialization,
        Self::CliRecoveryCslLogRead,
        Self::CliRecoveryCslLogDeserialization,
        Self::CliRecoveryCslSnapshotClone,
        Self::CliRecoveryCslSnapshotSerialization,
        Self::CliResumeCanonicalHistoryClone,
        Self::CliResumeHistoryMaterialization,
        Self::CliJournalHistoryHydration,
        Self::CliJournalDigestMaterialization,
        Self::CliJournalDigestSerialization,
        Self::CliSessionMemoryShutdownMaterialization,
        Self::CliSessionRestoreHydration,
        Self::CliSlashForkCanonicalHistoryClone,
        Self::CliSlashForkJournalHistoryClone,
        Self::CliSlashForkRollbackSnapshot,
        Self::CliSubrunPromptPayloadClone,
        Self::CliTaskBackgroundHistoryClone,
        Self::CliTaskBackgroundHistoryQueue,
        Self::CliTurnRetryHistoryClone,
        Self::CliTurnUserInputProjection,
        Self::CslMaterializedStateClone,
        Self::CslPersistInputClone,
        Self::CslStateInstallClone,
        Self::CslSnapshotClone,
        Self::CslMessageHash,
        Self::CslHashIndexClone,
        Self::CslFileRead,
        Self::CslFileAppendSerialization,
        Self::CslFileRewriteSerialization,
        Self::CslDatabaseRead,
        Self::CslDatabaseSerialization,
        Self::CslDatabaseRows,
        Self::PromptDeltaHash,
        Self::PromptDeltaRead,
        Self::PromptDeltaRows,
        Self::PromptDeltaUnchangedPrefix,
        Self::RunAdmission,
        Self::FinalizationCheckpointClone,
        Self::FinalizationRecoveryComparison,
        Self::PipelineHeavyCheckpointClone,
        Self::PipelineCheckpointSerialization,
        Self::PipelineCheckpointRead,
        Self::PipelineCompositeIndexSerialization,
        Self::PipelineCompositeIndexRead,
        Self::PipelineRecoveryClone,
        Self::PipelineRecoverySerialization,
        Self::PipelineRecoveryHash,
        Self::PipelineEventJournalRead,
        Self::ContextBinding,
        Self::ContextOptimization,
        Self::ContextSerialization,
        Self::RuntimeContextMaterialization,
        Self::TurnTraceHistoryClone,
        Self::PromptCacheHistoryScan,
        Self::HistoryBudgetEstimationSerialization,
        Self::LlmWireTraceClone,
        Self::LlmWireTraceHash,
        Self::ProviderWireAssembly,
        Self::ProviderBodySerialization,
        Self::ProviderRetryRetention,
        Self::LlmCaptureArtifactClone,
        Self::LlmCaptureArtifactSerialization,
        Self::BridgePipelineInputMaterialization,
        Self::BridgeJournalReplayClone,
        Self::BridgeJournalReplaySerialization,
        Self::BridgeRequestCaptureClone,
        Self::BridgeDisconnectCaptureClone,
        Self::BridgeCompactionFixedContextClone,
        Self::ServerCslPersistClone,
        Self::ServerObserverQueue,
        Self::ServerCompactionFixedContextClone,
        Self::ServerToolPolicySchemaClone,
        Self::ServerToolAdmissionSnapshotClone,
        Self::ServerToolSchemaEstimationSerialization,
        Self::DelegationContextClone,
        Self::DelegationRetryContextClone,
        Self::DelegationParentMessagesClone,
        Self::ObservabilityRollbackSnapshotClone,
        Self::ObservabilityRollbackRestoreClone,
        Self::MemoryExtractionHistoryClone,
        Self::MemoryExtractionPromptSanitization,
        Self::MemoryExtractionPayloadSerialization,
        Self::MemoryExtractionQueue,
        Self::ForkPrefixMaterialization,
        Self::ForkPrefixHash,
        Self::ForkToolSchemaHash,
        Self::ForkPrefixReconstruction,
        Self::ServerForkToolSchemaClone,
        Self::ServerForkPrefixSerialization,
        Self::ServerRequestCaptureClone,
        Self::ServerContextTraceClone,
        Self::ToolSchemaCacheStabilizationClone,
        Self::ToolSchemaWireSortSerialization,
        Self::SessionRestoreHydration,
        Self::SessionRestoreTranscriptHydration,
        Self::ResumeHintHistoryClone,
        Self::TailPersistHistoryClone,
        Self::CompactionHistoryClone,
        Self::CompactionHistorySerialization,
        Self::RequestDumpClone,
        Self::RequestDumpSerialization,
        Self::CloudHistoryGroupingClone,
        Self::CloudSummaryPromptClone,
        Self::CloudSummarySerialization,
        Self::SyncOutboxPayloadClone,
        Self::SyncOutboxPayloadHash,
        Self::SyncOutboxJournalDeltaClone,
        Self::SyncOutboxStateClone,
        Self::SyncOutboxDeliveryClone,
        Self::SyncOutboxDeliverySerialization,
        Self::SyncOutboxQueueRead,
        Self::SyncOutboxQueueRewrite,
        Self::SessionJournalHistorySerialization,
        Self::SessionJournalFullRead,
        Self::SessionJournalTailRead,
        Self::SessionJournalDigestRead,
        Self::SessionJournalAppendDeltaRead,
        Self::EventIngestionJournalHash,
        Self::EventIngestionHistoryClone,
        Self::EventIngestionQueue,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgenticRequestSnapshot => "agentic_request_snapshot",
            Self::CliPostCommitSnapshot => "cli_post_commit_snapshot",
            Self::CliSettlementRollbackSnapshot => "cli_settlement_rollback_snapshot",
            Self::CliPrimaryCommitWorkerHistoryQueue => "cli_primary_commit_worker_history_queue",
            Self::CliPrimaryCommitWorkerFinalMessagesQueue => {
                "cli_primary_commit_worker_final_messages_queue"
            }
            Self::CliDebugCheckpointRead => "cli_debug_checkpoint_read",
            Self::CliDebugCheckpointDeserialization => "cli_debug_checkpoint_deserialization",
            Self::CliDebugCheckpointHistoryClone => "cli_debug_checkpoint_history_clone",
            Self::CliDebugHistoryDeltaClone => "cli_debug_history_delta_clone",
            Self::CliDebugHistoryDeltaComparison => "cli_debug_history_delta_comparison",
            Self::CliDebugDumpPayloadClone => "cli_debug_dump_payload_clone",
            Self::CliDebugDumpSerialization => "cli_debug_dump_serialization",
            Self::CliContextDumpJournalHistoryMaterialization => {
                "cli_context_dump_journal_history_materialization"
            }
            Self::CliContextDumpSerialization => "cli_context_dump_serialization",
            Self::CliCompletionProxyMessageClone => "cli_completion_proxy_message_clone",
            Self::CliDisplayHistoryProjectionClone => "cli_display_history_projection_clone",
            Self::CliForkChildHistoryMaterialization => "cli_fork_child_history_materialization",
            Self::CliForkFrozenToolSchemaClone => "cli_fork_frozen_tool_schema_clone",
            Self::CliForkPrefixSerialization => "cli_fork_prefix_serialization",
            Self::CliForkToolSchemaSerialization => "cli_fork_tool_schema_serialization",
            Self::CliHistoryEditMemoryMaterialization => "cli_history_edit_memory_materialization",
            Self::CliHistoryEditRollbackSnapshot => "cli_history_edit_rollback_snapshot",
            Self::CliManualCompactionHistoryMaterialization => {
                "cli_manual_compaction_history_materialization"
            }
            Self::CliManualCompactionRetainedHistoryClone => {
                "cli_manual_compaction_retained_history_clone"
            }
            Self::CliManualCompactionSwapProjection => "cli_manual_compaction_swap_projection",
            Self::CliMemoryInferenceRequestClone => "cli_memory_inference_request_clone",
            Self::CliOneShotContinuationClone => "cli_one_shot_continuation_clone",
            Self::CliPlanBackgroundHistoryClone => "cli_plan_background_history_clone",
            Self::CliPlanBackgroundHistoryQueue => "cli_plan_background_history_queue",
            Self::CliPostCommitQueue => "cli_post_commit_queue",
            Self::CliPromptContinuationSanitization => "cli_prompt_continuation_sanitization",
            Self::CliPromptHistoryMaterialization => "cli_prompt_history_materialization",
            Self::CliPromptNormalizationClone => "cli_prompt_normalization_clone",
            Self::CliPromptPayloadClone => "cli_prompt_payload_clone",
            Self::CliRecoveryCheckpointHistoryMaterialization => {
                "cli_recovery_checkpoint_history_materialization"
            }
            Self::CliRecoveryCslBackupRead => "cli_recovery_csl_backup_read",
            Self::CliRecoveryCslHistoryMaterialization => {
                "cli_recovery_csl_history_materialization"
            }
            Self::CliRecoveryCslLogRead => "cli_recovery_csl_log_read",
            Self::CliRecoveryCslLogDeserialization => "cli_recovery_csl_log_deserialization",
            Self::CliRecoveryCslSnapshotClone => "cli_recovery_csl_snapshot_clone",
            Self::CliRecoveryCslSnapshotSerialization => "cli_recovery_csl_snapshot_serialization",
            Self::CliResumeCanonicalHistoryClone => "cli_resume_canonical_history_clone",
            Self::CliResumeHistoryMaterialization => "cli_resume_history_materialization",
            Self::CliJournalHistoryHydration => "cli_journal_history_hydration",
            Self::CliJournalDigestMaterialization => "cli_journal_digest_materialization",
            Self::CliJournalDigestSerialization => "cli_journal_digest_serialization",
            Self::CliSessionMemoryShutdownMaterialization => {
                "cli_session_memory_shutdown_materialization"
            }
            Self::CliSessionRestoreHydration => "cli_session_restore_hydration",
            Self::CliSlashForkCanonicalHistoryClone => "cli_slash_fork_canonical_history_clone",
            Self::CliSlashForkJournalHistoryClone => "cli_slash_fork_journal_history_clone",
            Self::CliSlashForkRollbackSnapshot => "cli_slash_fork_rollback_snapshot",
            Self::CliSubrunPromptPayloadClone => "cli_subrun_prompt_payload_clone",
            Self::CliTaskBackgroundHistoryClone => "cli_task_background_history_clone",
            Self::CliTaskBackgroundHistoryQueue => "cli_task_background_history_queue",
            Self::CliTurnRetryHistoryClone => "cli_turn_retry_history_clone",
            Self::CliTurnUserInputProjection => "cli_turn_user_input_projection",
            Self::CslMaterializedStateClone => "csl_materialized_state_clone",
            Self::CslPersistInputClone => "csl_persist_input_clone",
            Self::CslStateInstallClone => "csl_state_install_clone",
            Self::CslSnapshotClone => "csl_snapshot_clone",
            Self::CslMessageHash => "csl_message_hash",
            Self::CslHashIndexClone => "csl_hash_index_clone",
            Self::CslFileRead => "csl_file_read",
            Self::CslFileAppendSerialization => "csl_file_append_serialization",
            Self::CslFileRewriteSerialization => "csl_file_rewrite_serialization",
            Self::CslDatabaseRead => "csl_database_read",
            Self::CslDatabaseSerialization => "csl_database_serialization",
            Self::CslDatabaseRows => "csl_database_rows",
            Self::PromptDeltaHash => "prompt_delta_hash",
            Self::PromptDeltaRead => "prompt_delta_read",
            Self::PromptDeltaRows => "prompt_delta_rows",
            Self::PromptDeltaUnchangedPrefix => "prompt_delta_unchanged_prefix",
            Self::RunAdmission => "run_admission",
            Self::FinalizationCheckpointClone => "finalization_checkpoint_clone",
            Self::FinalizationRecoveryComparison => "finalization_recovery_comparison",
            Self::PipelineHeavyCheckpointClone => "pipeline_heavy_checkpoint_clone",
            Self::PipelineCheckpointSerialization => "pipeline_checkpoint_serialization",
            Self::PipelineCheckpointRead => "pipeline_checkpoint_read",
            Self::PipelineCompositeIndexSerialization => "pipeline_composite_index_serialization",
            Self::PipelineCompositeIndexRead => "pipeline_composite_index_read",
            Self::PipelineRecoveryClone => "pipeline_recovery_clone",
            Self::PipelineRecoverySerialization => "pipeline_recovery_serialization",
            Self::PipelineRecoveryHash => "pipeline_recovery_hash",
            Self::PipelineEventJournalRead => "pipeline_event_journal_read",
            Self::ContextBinding => "context_binding",
            Self::ContextOptimization => "context_optimization",
            Self::ContextSerialization => "context_serialization",
            Self::RuntimeContextMaterialization => "runtime_context_materialization",
            Self::TurnTraceHistoryClone => "turn_trace_history_clone",
            Self::PromptCacheHistoryScan => "prompt_cache_history_scan",
            Self::HistoryBudgetEstimationSerialization => "history_budget_estimation_serialization",
            Self::LlmWireTraceClone => "llm_wire_trace_clone",
            Self::LlmWireTraceHash => "llm_wire_trace_hash",
            Self::ProviderWireAssembly => "provider_wire_assembly",
            Self::ProviderBodySerialization => "provider_body_serialization",
            Self::ProviderRetryRetention => "provider_retry_retention",
            Self::LlmCaptureArtifactClone => "llm_capture_artifact_clone",
            Self::LlmCaptureArtifactSerialization => "llm_capture_artifact_serialization",
            Self::BridgePipelineInputMaterialization => "bridge_pipeline_input_materialization",
            Self::BridgeJournalReplayClone => "bridge_journal_replay_clone",
            Self::BridgeJournalReplaySerialization => "bridge_journal_replay_serialization",
            Self::BridgeRequestCaptureClone => "bridge_request_capture_clone",
            Self::BridgeDisconnectCaptureClone => "bridge_disconnect_capture_clone",
            Self::BridgeCompactionFixedContextClone => "bridge_compaction_fixed_context_clone",
            Self::ServerCslPersistClone => "server_csl_persist_clone",
            Self::ServerObserverQueue => "server_observer_queue",
            Self::ServerCompactionFixedContextClone => "server_compaction_fixed_context_clone",
            Self::ServerToolPolicySchemaClone => "server_tool_policy_schema_clone",
            Self::ServerToolAdmissionSnapshotClone => "server_tool_admission_snapshot_clone",
            Self::ServerToolSchemaEstimationSerialization => {
                "server_tool_schema_estimation_serialization"
            }
            Self::DelegationContextClone => "delegation_context_clone",
            Self::DelegationRetryContextClone => "delegation_retry_context_clone",
            Self::DelegationParentMessagesClone => "delegation_parent_messages_clone",
            Self::ObservabilityRollbackSnapshotClone => "observability_rollback_snapshot_clone",
            Self::ObservabilityRollbackRestoreClone => "observability_rollback_restore_clone",
            Self::MemoryExtractionHistoryClone => "memory_extraction_history_clone",
            Self::MemoryExtractionPromptSanitization => "memory_extraction_prompt_sanitization",
            Self::MemoryExtractionPayloadSerialization => "memory_extraction_payload_serialization",
            Self::MemoryExtractionQueue => "memory_extraction_queue",
            Self::ForkPrefixMaterialization => "fork_prefix_materialization",
            Self::ForkPrefixHash => "fork_prefix_hash",
            Self::ForkToolSchemaHash => "fork_tool_schema_hash",
            Self::ForkPrefixReconstruction => "fork_prefix_reconstruction",
            Self::ServerForkToolSchemaClone => "server_fork_tool_schema_clone",
            Self::ServerForkPrefixSerialization => "server_fork_prefix_serialization",
            Self::ServerRequestCaptureClone => "server_request_capture_clone",
            Self::ServerContextTraceClone => "server_context_trace_clone",
            Self::ToolSchemaCacheStabilizationClone => "tool_schema_cache_stabilization_clone",
            Self::ToolSchemaWireSortSerialization => "tool_schema_wire_sort_serialization",
            Self::SessionRestoreHydration => "session_restore_hydration",
            Self::SessionRestoreTranscriptHydration => "session_restore_transcript_hydration",
            Self::ResumeHintHistoryClone => "resume_hint_history_clone",
            Self::TailPersistHistoryClone => "tail_persist_history_clone",
            Self::CompactionHistoryClone => "compaction_history_clone",
            Self::CompactionHistorySerialization => "compaction_history_serialization",
            Self::RequestDumpClone => "request_dump_clone",
            Self::RequestDumpSerialization => "request_dump_serialization",
            Self::CloudHistoryGroupingClone => "cloud_history_grouping_clone",
            Self::CloudSummaryPromptClone => "cloud_summary_prompt_clone",
            Self::CloudSummarySerialization => "cloud_summary_serialization",
            Self::SyncOutboxPayloadClone => "sync_outbox_payload_clone",
            Self::SyncOutboxPayloadHash => "sync_outbox_payload_hash",
            Self::SyncOutboxJournalDeltaClone => "sync_outbox_journal_delta_clone",
            Self::SyncOutboxStateClone => "sync_outbox_state_clone",
            Self::SyncOutboxDeliveryClone => "sync_outbox_delivery_clone",
            Self::SyncOutboxDeliverySerialization => "sync_outbox_delivery_serialization",
            Self::SyncOutboxQueueRead => "sync_outbox_queue_read",
            Self::SyncOutboxQueueRewrite => "sync_outbox_queue_rewrite",
            Self::SessionJournalHistorySerialization => "session_journal_history_serialization",
            Self::SessionJournalFullRead => "session_journal_full_read",
            Self::SessionJournalTailRead => "session_journal_tail_read",
            Self::SessionJournalDigestRead => "session_journal_digest_read",
            Self::SessionJournalAppendDeltaRead => "session_journal_append_delta_read",
            Self::EventIngestionJournalHash => "event_ingestion_journal_hash",
            Self::EventIngestionHistoryClone => "event_ingestion_history_clone",
            Self::EventIngestionQueue => "event_ingestion_queue",
        }
    }

    /// Module that owns this instrumented work site today.
    ///
    /// This is deliberately a fixed code inventory, not a caller-supplied
    /// label. It keeps the Phase-0 baseline low-cardinality. This table is an
    /// incremental inventory of instrumented sites, not a claim that every
    /// production O(history) path has already been discovered.
    pub const fn owner(self) -> &'static str {
        match self {
            Self::AgenticRequestSnapshot => "runtime.agentic_loop",
            Self::CliPostCommitSnapshot
            | Self::CliSettlementRollbackSnapshot
            | Self::CliPrimaryCommitWorkerHistoryQueue
            | Self::CliPrimaryCommitWorkerFinalMessagesQueue
            | Self::CliPostCommitQueue => "cli.turn_settlement",
            Self::CliDebugCheckpointRead
            | Self::CliDebugCheckpointDeserialization
            | Self::CliDebugCheckpointHistoryClone
            | Self::CliDebugHistoryDeltaClone
            | Self::CliDebugHistoryDeltaComparison
            | Self::CliDebugDumpPayloadClone
            | Self::CliDebugDumpSerialization => "cli.slash.debug",
            Self::CliContextDumpJournalHistoryMaterialization
            | Self::CliContextDumpSerialization => "cli.context_dump",
            Self::CliCompletionProxyMessageClone => "cli.chat_stream.proxy_completion",
            Self::CliDisplayHistoryProjectionClone | Self::CliSessionRestoreHydration => {
                "cli.session_continuation"
            }
            Self::CliForkChildHistoryMaterialization => "cli.spawn_subrun",
            Self::CliForkFrozenToolSchemaClone | Self::CliSubrunPromptPayloadClone => {
                "cli.skill_subrun"
            }
            Self::CliForkPrefixSerialization | Self::CliForkToolSchemaSerialization => {
                "cli.chat_stream.fork_capture"
            }
            Self::CliHistoryEditMemoryMaterialization
            | Self::CliHistoryEditRollbackSnapshot
            | Self::CliManualCompactionHistoryMaterialization
            | Self::CliManualCompactionRetainedHistoryClone
            | Self::CliManualCompactionSwapProjection => "cli.slash.history_edit",
            Self::CliMemoryInferenceRequestClone => "cli.session.memory_inference",
            Self::CliOneShotContinuationClone => "cli.one_shot_session_routing",
            Self::CliPlanBackgroundHistoryClone | Self::CliPlanBackgroundHistoryQueue => {
                "cli.plan_executor"
            }
            Self::CliPromptContinuationSanitization | Self::CliPromptHistoryMaterialization => {
                "cli.chat_stream.load_turn_messages"
            }
            Self::CliPromptNormalizationClone | Self::CliPromptPayloadClone => {
                "cli.chat_stream.prepare_payload"
            }
            Self::CliRecoveryCheckpointHistoryMaterialization
            | Self::CliRecoveryCslBackupRead
            | Self::CliRecoveryCslHistoryMaterialization
            | Self::CliRecoveryCslLogRead
            | Self::CliRecoveryCslLogDeserialization
            | Self::CliRecoveryCslSnapshotClone
            | Self::CliRecoveryCslSnapshotSerialization => "cli.session_recovery",
            Self::CliResumeCanonicalHistoryClone | Self::CliResumeHistoryMaterialization => {
                "cli.session_resume"
            }
            Self::CliJournalHistoryHydration => "cli.session_runtime",
            Self::CliJournalDigestMaterialization | Self::CliJournalDigestSerialization => {
                "cli.journal_digest"
            }
            Self::CliSessionMemoryShutdownMaterialization => "cli.session_cleanup",
            Self::CliSlashForkCanonicalHistoryClone
            | Self::CliSlashForkJournalHistoryClone
            | Self::CliSlashForkRollbackSnapshot => "cli.slash.session_fork",
            Self::CliTaskBackgroundHistoryClone | Self::CliTaskBackgroundHistoryQueue => {
                "cli.slash_task"
            }
            Self::CliTurnRetryHistoryClone => "cli.turn_facade",
            Self::CliTurnUserInputProjection => "cli.stream_settlement",
            Self::CslMaterializedStateClone
            | Self::CslPersistInputClone
            | Self::CslStateInstallClone
            | Self::CslSnapshotClone
            | Self::CslMessageHash
            | Self::CslHashIndexClone => "turn_core.csl_manager",
            Self::CslFileRead
            | Self::CslFileAppendSerialization
            | Self::CslFileRewriteSerialization => "turn_core.csl_file_store",
            Self::CslDatabaseRead | Self::CslDatabaseSerialization | Self::CslDatabaseRows => {
                "turn_core.csl_database_store"
            }
            Self::PromptDeltaHash | Self::PromptDeltaRead | Self::PromptDeltaRows => {
                "services.prompt_delta"
            }
            Self::RunAdmission => "runtime.run_admission",
            Self::PromptDeltaUnchangedPrefix => "services.prompt_delta",
            Self::FinalizationCheckpointClone | Self::FinalizationRecoveryComparison => {
                "runtime.agentic_loop.finalization"
            }
            Self::PipelineHeavyCheckpointClone
            | Self::PipelineCheckpointSerialization
            | Self::PipelineCheckpointRead
            | Self::PipelineCompositeIndexSerialization
            | Self::PipelineCompositeIndexRead
            | Self::PipelineRecoveryClone => "pipeline.persistence",
            Self::PipelineRecoverySerialization | Self::PipelineRecoveryHash => {
                "pipeline.crash_recovery"
            }
            Self::PipelineEventJournalRead => "pipeline.event_store",
            Self::ContextBinding => "turn_core.context_binder",
            Self::ContextOptimization => "turn_core.context_optimizer",
            Self::ContextSerialization => "turn_core.context_serializer",
            Self::RuntimeContextMaterialization => "runtime.context",
            Self::TurnTraceHistoryClone => "turn_core.turn_trace_collector",
            Self::PromptCacheHistoryScan => "turn_core.prompt_cache_diagnostics",
            Self::HistoryBudgetEstimationSerialization => "runtime.turn.wire_assembly",
            Self::LlmWireTraceClone | Self::LlmWireTraceHash => "runtime.turn.llm_context",
            Self::ProviderWireAssembly
            | Self::ProviderBodySerialization
            | Self::ProviderRetryRetention => "runtime.turn.bridge",
            Self::LlmCaptureArtifactClone | Self::LlmCaptureArtifactSerialization => {
                "runtime.turn.llm_capture"
            }
            Self::BridgePipelineInputMaterialization => "runtime.turn.prompt_cache",
            Self::BridgeJournalReplayClone => "runtime.turn.bridge.journal_replay",
            Self::BridgeJournalReplaySerialization => "runtime.turn.bridge.journal_replay",
            Self::BridgeRequestCaptureClone => "runtime.turn.bridge.llm_capture",
            Self::BridgeDisconnectCaptureClone => "runtime.turn.bridge.disconnect_capture",
            Self::BridgeCompactionFixedContextClone => "runtime.turn.bridge.compaction",
            Self::ServerCslPersistClone => "runtime.server.csl",
            Self::ServerObserverQueue => "runtime.server.observer",
            Self::ServerCompactionFixedContextClone => "runtime.server.compaction",
            Self::ServerToolPolicySchemaClone => "runtime.server.tool_policy",
            Self::ServerToolAdmissionSnapshotClone => "runtime.server.tool_admission_snapshot",
            Self::ServerToolSchemaEstimationSerialization => {
                "runtime.server.tool_schema_estimation"
            }
            Self::DelegationContextClone
            | Self::DelegationRetryContextClone
            | Self::DelegationParentMessagesClone => "runtime.server.delegation",
            Self::ObservabilityRollbackSnapshotClone | Self::ObservabilityRollbackRestoreClone => {
                "runtime.observability.rollback"
            }
            Self::MemoryExtractionHistoryClone | Self::MemoryExtractionQueue => {
                "runtime.services.memory"
            }
            Self::MemoryExtractionPromptSanitization => "runtime.session_memory.runner",
            Self::MemoryExtractionPayloadSerialization => "runtime.turn.services.memory",
            Self::ForkPrefixMaterialization => "turn_core.csl_fork",
            Self::ForkPrefixHash | Self::ForkToolSchemaHash => "turn_core.fork_prefix",
            Self::ForkPrefixReconstruction => "runtime.orchestration.fork_reconstruct",
            Self::ServerForkToolSchemaClone | Self::ServerForkPrefixSerialization => {
                "runtime.server.fork_prefix"
            }
            Self::ServerRequestCaptureClone => "runtime.server.llm_capture",
            Self::ServerContextTraceClone => "runtime.server.llm_trace",
            Self::ToolSchemaCacheStabilizationClone => "runtime.turn.llm_context",
            Self::ToolSchemaWireSortSerialization => "runtime.turn.llm_context",
            Self::SessionRestoreHydration | Self::SessionRestoreTranscriptHydration => {
                "services.session_restore"
            }
            Self::ResumeHintHistoryClone => "turn_core.resume_hydration",
            Self::TailPersistHistoryClone => "turn_core.tail_persist",
            Self::CompactionHistoryClone => "turn_core.compaction",
            Self::CompactionHistorySerialization => "runtime.turn.cloud.compaction",
            Self::RequestDumpClone | Self::RequestDumpSerialization => "turn_core.llm_request_dump",
            Self::CloudHistoryGroupingClone => "turn_core.cloud_grouping",
            Self::CloudSummaryPromptClone | Self::CloudSummarySerialization => {
                "turn_core.cloud_summary"
            }
            Self::SyncOutboxPayloadClone
            | Self::SyncOutboxPayloadHash
            | Self::SyncOutboxJournalDeltaClone
            | Self::SyncOutboxStateClone
            | Self::SyncOutboxDeliveryClone
            | Self::SyncOutboxDeliverySerialization
            | Self::SyncOutboxQueueRead
            | Self::SyncOutboxQueueRewrite => "services.sync_outbox",
            Self::SessionJournalHistorySerialization
            | Self::SessionJournalFullRead
            | Self::SessionJournalTailRead
            | Self::SessionJournalDigestRead
            | Self::SessionJournalAppendDeltaRead => "services.session_journal",
            Self::EventIngestionJournalHash
            | Self::EventIngestionHistoryClone
            | Self::EventIngestionQueue => "services.event_ingestion",
        }
    }

    /// First plan phase expected to govern this site's normal-path redesign.
    ///
    /// A storage primitive can also be reached by fork, recovery, or
    /// diagnostic paths that have a later phase. Those path-specific sites
    /// are split out as the Phase-0 inventory expands, so this must not be
    /// interpreted as a guarantee that all work under the owner disappears in
    /// this phase.
    pub const fn primary_target_phase(self) -> u8 {
        match self {
            // Phase 1 commits before installing live state, removing the
            // whole-session rollback copy from turn settlement.
            Self::CliSettlementRollbackSnapshot
            | Self::CliTurnRetryHistoryClone
            | Self::CliTurnUserInputProjection
            | Self::ObservabilityRollbackSnapshotClone
            | Self::ObservabilityRollbackRestoreClone => 1,
            Self::SessionRestoreHydration
            | Self::SessionRestoreTranscriptHydration
            | Self::ResumeHintHistoryClone
            | Self::CliDisplayHistoryProjectionClone
            | Self::CliOneShotContinuationClone
            | Self::CliRecoveryCheckpointHistoryMaterialization
            | Self::CliRecoveryCslBackupRead
            | Self::CliRecoveryCslHistoryMaterialization
            | Self::CliRecoveryCslLogRead
            | Self::CliRecoveryCslSnapshotClone
            | Self::CliRecoveryCslSnapshotSerialization
            | Self::CliResumeCanonicalHistoryClone
            | Self::CliResumeHistoryMaterialization
            | Self::CliSessionRestoreHydration
            | Self::CliJournalHistoryHydration => 2,
            Self::ForkPrefixMaterialization
            | Self::ForkPrefixHash
            | Self::ForkToolSchemaHash
            | Self::ForkPrefixReconstruction
            | Self::ServerForkToolSchemaClone
            | Self::ServerForkPrefixSerialization
            | Self::CliForkChildHistoryMaterialization
            | Self::CliForkFrozenToolSchemaClone
            | Self::CliForkPrefixSerialization
            | Self::CliForkToolSchemaSerialization
            | Self::CliSlashForkCanonicalHistoryClone
            | Self::CliSlashForkJournalHistoryClone
            | Self::CliSlashForkRollbackSnapshot
            | Self::DelegationParentMessagesClone => 5,
            Self::RuntimeContextMaterialization
            | Self::PromptCacheHistoryScan
            | Self::HistoryBudgetEstimationSerialization
            | Self::CompactionHistoryClone
            | Self::CompactionHistorySerialization
            | Self::CloudHistoryGroupingClone
            | Self::CloudSummaryPromptClone
            | Self::CloudSummarySerialization
            | Self::CliCompletionProxyMessageClone
            | Self::CliManualCompactionHistoryMaterialization
            | Self::CliManualCompactionRetainedHistoryClone
            | Self::CliManualCompactionSwapProjection
            | Self::CliPromptContinuationSanitization
            | Self::CliPromptHistoryMaterialization
            | Self::CliPromptNormalizationClone
            | Self::CliPromptPayloadClone
            | Self::CliSubrunPromptPayloadClone
            | Self::BridgePipelineInputMaterialization
            | Self::BridgeCompactionFixedContextClone
            | Self::ServerCompactionFixedContextClone
            | Self::ServerToolPolicySchemaClone
            | Self::ServerToolSchemaEstimationSerialization
            | Self::MemoryExtractionPromptSanitization
            | Self::ToolSchemaCacheStabilizationClone
            | Self::ToolSchemaWireSortSerialization => 6,
            Self::LlmWireTraceClone
            | Self::LlmWireTraceHash
            | Self::LlmCaptureArtifactClone
            | Self::LlmCaptureArtifactSerialization
            | Self::ServerRequestCaptureClone
            | Self::ServerContextTraceClone
            | Self::RequestDumpClone
            | Self::RequestDumpSerialization
            | Self::SessionJournalHistorySerialization
            | Self::SessionJournalTailRead
            | Self::SessionJournalDigestRead
            | Self::BridgeJournalReplayClone
            | Self::BridgeJournalReplaySerialization
            | Self::BridgeRequestCaptureClone
            | Self::BridgeDisconnectCaptureClone
            | Self::ServerToolAdmissionSnapshotClone
            | Self::CliDebugCheckpointRead
            | Self::CliDebugCheckpointDeserialization
            | Self::CliDebugCheckpointHistoryClone
            | Self::CliDebugHistoryDeltaClone
            | Self::CliDebugHistoryDeltaComparison
            | Self::CliDebugDumpPayloadClone
            | Self::CliDebugDumpSerialization
            | Self::CliContextDumpJournalHistoryMaterialization
            | Self::CliContextDumpSerialization
            | Self::CliJournalDigestMaterialization
            | Self::CliJournalDigestSerialization
            | Self::TurnTraceHistoryClone => 7,
            // Phase 3 introduces immutable segments, manifest/delta
            // post-commit jobs, O(1) unchanged-prefix prompt deltas, and
            // weighted tenant admission.
            _ => 3,
        }
    }

    pub const fn primary_target_phase_label(self) -> &'static str {
        match self.primary_target_phase() {
            1 => "1",
            2 => "2",
            3 => "3",
            4 => "4",
            5 => "5",
            6 => "6",
            7 => "7",
            _ => "unassigned",
        }
    }
}

/// The independently audited Phase-0 inventory covers every identified
/// production O(history) clone, hash, serialization, write, queue, row, and
/// admission boundary. Keep this declaration machine-readable so production
/// baseline verification fails closed if a future audit reopens coverage.
pub const HISTORY_WORK_COVERAGE_COMPLETE: bool = true;
const _: () = assert!(HISTORY_WORK_COVERAGE_COMPLETE);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryWorkKnownOmission {
    pub key: &'static str,
    pub owner: &'static str,
    pub target_phase: u8,
    pub reason: &'static str,
}

/// Known production work intentionally omitted from [`HistoryWorkSite::ALL`].
///
/// The completed independent audit found no such omissions. A future omission
/// must be listed here and must reopen [`HISTORY_WORK_COVERAGE_COMPLETE`] until
/// it has an authoritative production recorder.
pub const HISTORY_WORK_KNOWN_OMISSIONS: &[HistoryWorkKnownOmission] = &[];

static EVENTS: [AtomicU64; HistoryWorkSite::COUNT] =
    [const { AtomicU64::new(0) }; HistoryWorkSite::COUNT];
static BYTES: [AtomicU64; HistoryWorkSite::COUNT] =
    [const { AtomicU64::new(0) }; HistoryWorkSite::COUNT];
static ROWS: [AtomicU64; HistoryWorkSite::COUNT] =
    [const { AtomicU64::new(0) }; HistoryWorkSite::COUNT];
static ADMISSION_UNITS: [AtomicU64; HistoryWorkSite::COUNT] =
    [const { AtomicU64::new(0) }; HistoryWorkSite::COUNT];
static QUEUE_CURRENT_BYTES: [AtomicU64; HistoryWorkSite::COUNT] =
    [const { AtomicU64::new(0) }; HistoryWorkSite::COUNT];
static QUEUE_PEAK_BYTES: [AtomicU64; HistoryWorkSite::COUNT] =
    [const { AtomicU64::new(0) }; HistoryWorkSite::COUNT];
static ACCOUNTING_ERRORS: [AtomicU64; HistoryWorkSite::COUNT] =
    [const { AtomicU64::new(0) }; HistoryWorkSite::COUNT];
static NEXT_SCENARIO_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_SCENARIO_ID: AtomicU64 = AtomicU64::new(0);
static ACTIVE_SCENARIO: LazyLock<Mutex<Option<ActiveHistoryWorkScenario>>> =
    LazyLock::new(|| Mutex::new(None));

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HistoryWorkMeasurement {
    pub events: u64,
    pub bytes: u64,
    pub rows: u64,
    pub admission_units: u64,
    pub queue_current_bytes: u64,
    pub queue_peak_bytes: u64,
    /// Counter saturation or measurement failures. A non-zero value means
    /// byte/row/queue measurements are incomplete and must not be used as a
    /// quantitative baseline.
    pub accounting_errors: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HistoryWorkMeasurementDelta {
    pub events: u64,
    pub bytes: u64,
    pub rows: u64,
    pub admission_units: u64,
    /// Signed because a scenario can release a reservation created before its
    /// baseline snapshot.
    pub queue_current_bytes_change: i128,
    /// Increase in the process-lifetime high-water mark. In a dedicated
    /// single-workload process, [`HistoryWorkScenarioReport::scoped`] provides
    /// the interval-local scenario peak. Production concurrency is not an
    /// exactly isolated interval.
    pub queue_peak_bytes_increase: u64,
    pub accounting_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryWorkSnapshot {
    pub sites: Vec<(HistoryWorkSite, HistoryWorkMeasurement)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryWorkDelta {
    pub sites: Vec<(HistoryWorkSite, HistoryWorkMeasurementDelta)>,
}

impl HistoryWorkSnapshot {
    pub fn capture() -> Self {
        Self {
            sites: HistoryWorkSite::ALL
                .into_iter()
                .map(|site| (site, measurement(site)))
                .collect(),
        }
    }

    pub fn measurement(&self, site: HistoryWorkSite) -> HistoryWorkMeasurement {
        self.sites
            .iter()
            .find_map(|(candidate, measurement)| (*candidate == site).then_some(*measurement))
            .unwrap_or_default()
    }

    /// Return the monotonic counter difference from an earlier snapshot.
    ///
    /// This is suitable for a dedicated-process, single-workload runner.
    /// Queue current bytes are signed, while the global peak field only
    /// reports a new process high-water mark. Snapshot capture and scenario
    /// activation are separate operations, so production-concurrent deltas
    /// are observational rather than transactionally exact.
    pub fn delta_since(&self, baseline: &Self) -> HistoryWorkDelta {
        HistoryWorkDelta {
            sites: HistoryWorkSite::ALL
                .into_iter()
                .map(|site| {
                    let before = baseline.measurement(site);
                    let after = self.measurement(site);
                    (
                        site,
                        HistoryWorkMeasurementDelta {
                            events: monotonic_counter_delta(
                                site,
                                "events",
                                before.events,
                                after.events,
                            ),
                            bytes: monotonic_counter_delta(
                                site,
                                "bytes",
                                before.bytes,
                                after.bytes,
                            ),
                            rows: monotonic_counter_delta(site, "rows", before.rows, after.rows),
                            admission_units: monotonic_counter_delta(
                                site,
                                "admission_units",
                                before.admission_units,
                                after.admission_units,
                            ),
                            queue_current_bytes_change: i128::from(after.queue_current_bytes)
                                - i128::from(before.queue_current_bytes),
                            queue_peak_bytes_increase: monotonic_counter_delta(
                                site,
                                "queue_peak_bytes",
                                before.queue_peak_bytes,
                                after.queue_peak_bytes,
                            ),
                            accounting_errors: monotonic_counter_delta(
                                site,
                                "accounting_errors",
                                before.accounting_errors,
                                after.accounting_errors,
                            ),
                        },
                    )
                })
                .collect(),
        }
    }
}

fn monotonic_counter_delta(site: HistoryWorkSite, counter: &str, before: u64, after: u64) -> u64 {
    debug_assert!(
        after >= before,
        "site {site:?} {counter} regressed: {after} < {before}"
    );
    after.saturating_sub(before)
}

impl HistoryWorkDelta {
    pub fn measurement(&self, site: HistoryWorkSite) -> HistoryWorkMeasurementDelta {
        self.sites
            .iter()
            .find_map(|(candidate, measurement)| (*candidate == site).then_some(*measurement))
            .unwrap_or_default()
    }
}

#[derive(Debug)]
struct ActiveHistoryWorkScenario {
    id: u64,
    label: String,
    sites: [HistoryWorkMeasurement; HistoryWorkSite::COUNT],
}

#[derive(Debug)]
pub struct HistoryWorkScenario {
    id: u64,
    label: String,
    started_at: Instant,
    global_before: HistoryWorkSnapshot,
    finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryWorkScenarioReport {
    pub id: u64,
    pub label: String,
    pub elapsed: Duration,
    pub global_before: HistoryWorkSnapshot,
    pub global_after: HistoryWorkSnapshot,
    pub global_delta: HistoryWorkDelta,
    /// Counters attributed while this exclusive process-local scenario was
    /// active. Queue current/peak values start at zero, so the peak is not
    /// polluted by an earlier scenario's process high-water mark. This is an
    /// exact workload interval only in a dedicated process with no concurrent
    /// production recorders.
    pub scoped: HistoryWorkSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryWorkScenarioError {
    EmptyLabel,
    AlreadyActive { id: u64, label: String },
    NoLongerActive { expected_id: u64 },
}

impl std::fmt::Display for HistoryWorkScenarioError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyLabel => formatter.write_str("history-work scenario label cannot be empty"),
            Self::AlreadyActive { id, label } => {
                write!(
                    formatter,
                    "history-work scenario {id} ({label}) is already active"
                )
            }
            Self::NoLongerActive { expected_id } => write!(
                formatter,
                "history-work scenario {expected_id} is no longer the active scenario"
            ),
        }
    }
}

impl std::error::Error for HistoryWorkScenarioError {}

impl HistoryWorkScenario {
    /// Begin one exclusive process-local measurement interval.
    ///
    /// Production calls that happen concurrently are deliberately included.
    /// Nested/overlapping scenarios are rejected, but begin/finish and global
    /// counter snapshots do not form one cross-counter transaction. Exact
    /// workload attribution therefore requires a dedicated process with no
    /// concurrent production recorders.
    pub fn begin(label: impl Into<String>) -> Result<Self, HistoryWorkScenarioError> {
        let label = label.into();
        if label.trim().is_empty() {
            return Err(HistoryWorkScenarioError::EmptyLabel);
        }

        let mut active = lock_active_scenario();
        if let Some(active) = active.as_ref() {
            return Err(HistoryWorkScenarioError::AlreadyActive {
                id: active.id,
                label: active.label.clone(),
            });
        }
        let id = next_scenario_id();
        let global_before = HistoryWorkSnapshot::capture();
        *active = Some(ActiveHistoryWorkScenario {
            id,
            label: label.clone(),
            sites: [HistoryWorkMeasurement::default(); HistoryWorkSite::COUNT],
        });
        ACTIVE_SCENARIO_ID.store(id, Ordering::Release);
        Ok(Self {
            id,
            label,
            started_at: Instant::now(),
            global_before,
            finished: false,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn finish(mut self) -> Result<HistoryWorkScenarioReport, HistoryWorkScenarioError> {
        let mut active = lock_active_scenario();
        let Some(current) = active.as_ref() else {
            return Err(HistoryWorkScenarioError::NoLongerActive {
                expected_id: self.id,
            });
        };
        if current.id != self.id {
            return Err(HistoryWorkScenarioError::NoLongerActive {
                expected_id: self.id,
            });
        }

        // Disable attribution while holding the same lock used by recorders.
        // Any recorder that linearizes after this point belongs to the next
        // scenario, not this one.
        ACTIVE_SCENARIO_ID.store(0, Ordering::Release);
        let current = active.take().expect("active scenario checked above");
        let global_after = HistoryWorkSnapshot::capture();
        let global_delta = global_after.delta_since(&self.global_before);
        self.finished = true;
        Ok(HistoryWorkScenarioReport {
            id: self.id,
            label: self.label.clone(),
            elapsed: self.started_at.elapsed(),
            global_before: self.global_before.clone(),
            global_after,
            global_delta,
            scoped: HistoryWorkSnapshot {
                sites: HistoryWorkSite::ALL
                    .into_iter()
                    .map(|site| (site, current.sites[site as usize]))
                    .collect(),
            },
        })
    }
}

impl Drop for HistoryWorkScenario {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut active = lock_active_scenario();
        if active.as_ref().is_some_and(|active| active.id == self.id) {
            ACTIVE_SCENARIO_ID.store(0, Ordering::Release);
            active.take();
            tracing::warn!(
                target: "astra::history_work",
                scenario_id = self.id,
                scenario = self.label,
                "aborted history-work scenario without a report"
            );
        }
    }
}

fn lock_active_scenario() -> MutexGuard<'static, Option<ActiveHistoryWorkScenario>> {
    ACTIVE_SCENARIO
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_scenario_id() -> u64 {
    loop {
        let id = NEXT_SCENARIO_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

pub fn instrumentation_enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var(TRACE_ENV)
            .ok()
            .is_some_and(|value| matches!(value.trim(), "1" | "true" | "on"))
    });
    *ENABLED
}

pub fn record_bytes(site: HistoryWorkSite, bytes: u64) {
    record_operation(site, bytes, 0, 0);
}

pub fn record_rows(site: HistoryWorkSite, rows: u64) {
    record_operation(site, 0, rows, 0);
}

pub fn record_admission_units(site: HistoryWorkSite, units: u64) {
    record_operation(site, 0, 0, units);
}

/// Record one observed operation with all of its measured dimensions.
///
/// Callers that know bytes and rows at the same boundary must use this instead
/// of calling the single-dimension helpers twice; `events` counts operations,
/// not counter updates.
pub fn record_operation(site: HistoryWorkSite, bytes: u64, rows: u64, admission_units: u64) {
    record_global_operation(site, bytes, rows, admission_units);
    record_scoped_operation(site, bytes, rows, admission_units);
    trace_operation(site, bytes, rows, admission_units);
}

fn record_global_operation(site: HistoryWorkSite, bytes: u64, rows: u64, admission_units: u64) {
    let saturated = [
        atomic_saturating_add(&EVENTS[site as usize], 1),
        atomic_saturating_add(&BYTES[site as usize], bytes),
        atomic_saturating_add(&ROWS[site as usize], rows),
        atomic_saturating_add(&ADMISSION_UNITS[site as usize], admission_units),
    ]
    .into_iter()
    .any(|saturated| saturated);
    if saturated {
        record_accounting_error(site);
    }
}

fn atomic_saturating_add(counter: &AtomicU64, value: u64) -> bool {
    let previous = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(value))
        })
        .expect("saturating atomic update cannot fail");
    previous.checked_add(value).is_none()
}

fn record_accounting_error(site: HistoryWorkSite) {
    record_global_accounting_error(site);
    record_scoped_accounting_error(site);
}

fn record_global_accounting_error(site: HistoryWorkSite) {
    let _ = ACCOUNTING_ERRORS[site as usize].fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |current| Some(current.saturating_add(1)),
    );
}

fn record_scoped_accounting_error(site: HistoryWorkSite) {
    let id = ACTIVE_SCENARIO_ID.load(Ordering::Acquire);
    if id == 0 {
        return;
    }
    let mut active = lock_active_scenario();
    let Some(active) = active.as_mut().filter(|active| active.id == id) else {
        return;
    };
    active.sites[site as usize].accounting_errors = active.sites[site as usize]
        .accounting_errors
        .saturating_add(1);
}

fn record_scoped_operation(site: HistoryWorkSite, bytes: u64, rows: u64, admission_units: u64) {
    let id = ACTIVE_SCENARIO_ID.load(Ordering::Acquire);
    if id == 0 {
        return;
    }
    let overflowed = {
        let mut active = lock_active_scenario();
        let Some(active) = active.as_mut().filter(|active| active.id == id) else {
            return;
        };
        let measurement = &mut active.sites[site as usize];
        let overflowed = [
            saturating_add_assign(&mut measurement.events, 1),
            saturating_add_assign(&mut measurement.bytes, bytes),
            saturating_add_assign(&mut measurement.rows, rows),
            saturating_add_assign(&mut measurement.admission_units, admission_units),
        ]
        .into_iter()
        .any(|overflowed| overflowed);
        if overflowed {
            measurement.accounting_errors = measurement.accounting_errors.saturating_add(1);
        }
        overflowed
    };
    if overflowed {
        record_global_accounting_error(site);
        tracing::error!(
            target: "astra::history_work",
            site = site.as_str(),
            "scoped history-work counters saturated; measurement is incomplete"
        );
    }
}

fn saturating_add_assign(current: &mut u64, value: u64) -> bool {
    match current.checked_add(value) {
        Some(updated) => {
            *current = updated;
            false
        }
        None => {
            *current = u64::MAX;
            true
        }
    }
}

fn trace_operation(site: HistoryWorkSite, bytes: u64, rows: u64, admission_units: u64) {
    tracing::debug!(
        target: "astra::history_work",
        site = site.as_str(),
        bytes,
        rows,
        admission_units,
        "observed history work"
    );
}

fn measurement(site: HistoryWorkSite) -> HistoryWorkMeasurement {
    HistoryWorkMeasurement {
        events: EVENTS[site as usize].load(Ordering::Relaxed),
        bytes: BYTES[site as usize].load(Ordering::Relaxed),
        rows: ROWS[site as usize].load(Ordering::Relaxed),
        admission_units: ADMISSION_UNITS[site as usize].load(Ordering::Relaxed),
        queue_current_bytes: QUEUE_CURRENT_BYTES[site as usize].load(Ordering::Relaxed),
        queue_peak_bytes: QUEUE_PEAK_BYTES[site as usize].load(Ordering::Relaxed),
        accounting_errors: ACCOUNTING_ERRORS[site as usize].load(Ordering::Relaxed),
    }
}

/// Count the exact compact-JSON bytes without retaining a second buffer.
///
/// This still performs O(history) serialization work and is therefore called
/// only when [`instrumentation_enabled`] is true or from an existing
/// serialization boundary that already owns the bytes.
pub fn serialized_bytes<T: Serialize + ?Sized>(value: &T) -> Result<u64, serde_json::Error> {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

/// Measure and record the compact-JSON bytes for one instrumented value.
///
/// This helper owns the instrumentation gate and error accounting so
/// production callers cannot silently discard serialization failures.
pub fn record_serialized_value<T: Serialize + ?Sized>(site: HistoryWorkSite, value: &T) {
    let Some(bytes) = measure_serialized_value(site, value) else {
        return;
    };
    record_bytes(site, bytes);
}

/// Mark an already-attempted JSON serialization as incomplete.
///
/// Use this at production serialization boundaries that need a functional
/// fallback after `serde_json` fails. The fallback may keep the application
/// running, but the instrumentation interval is invalidated.
pub fn record_serialization_failure(site: HistoryWorkSite, error: &serde_json::Error) {
    if !instrumentation_enabled() {
        return;
    }
    record_serialization_failure_enabled(site, error);
}

fn record_serialization_failure_enabled(site: HistoryWorkSite, error: &serde_json::Error) {
    record_accounting_error(site);
    tracing::error!(
        target: "astra::history_work",
        site = site.as_str(),
        %error,
        "history-work serialization measurement failed"
    );
}

fn measure_serialized_value<T: Serialize + ?Sized>(
    site: HistoryWorkSite,
    value: &T,
) -> Option<u64> {
    if !instrumentation_enabled() {
        return None;
    }
    measure_serialized_value_enabled(site, value)
}

fn measure_serialized_value_enabled<T: Serialize + ?Sized>(
    site: HistoryWorkSite,
    value: &T,
) -> Option<u64> {
    match serialized_bytes(value) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            record_serialization_failure_enabled(site, &error);
            None
        }
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let additional_bytes = u64::try_from(bytes.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "serialized byte count does not fit in u64",
            )
        })?;
        self.bytes = self.bytes.checked_add(additional_bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "serialized byte count overflowed u64",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// RAII accounting for a full-history payload retained by a bounded queue.
pub struct QueueBytesReservation {
    site: HistoryWorkSite,
    bytes: u64,
    scenario_id: u64,
    accounted: bool,
}

impl QueueBytesReservation {
    pub fn for_site(site: HistoryWorkSite, bytes: u64) -> Self {
        let Some(current) = try_reserve_queue_bytes(&QUEUE_CURRENT_BYTES[site as usize], bytes)
        else {
            record_accounting_error(site);
            tracing::error!(
                target: "astra::history_work",
                site = site.as_str(),
                bytes,
                "queue byte reservation overflowed; measurement is incomplete"
            );
            return Self {
                site,
                bytes,
                scenario_id: 0,
                accounted: false,
            };
        };
        QUEUE_PEAK_BYTES[site as usize].fetch_max(current, Ordering::AcqRel);

        record_global_operation(site, bytes, 0, 0);

        let scenario_id = ACTIVE_SCENARIO_ID.load(Ordering::Acquire);
        let mut attributed_scenario_id = 0;
        let mut scoped_overflowed = false;
        if scenario_id != 0 {
            let mut active = lock_active_scenario();
            if let Some(active) = active.as_mut().filter(|active| active.id == scenario_id) {
                let measurement = &mut active.sites[site as usize];
                scoped_overflowed = [
                    saturating_add_assign(&mut measurement.events, 1),
                    saturating_add_assign(&mut measurement.bytes, bytes),
                    saturating_add_assign(&mut measurement.queue_current_bytes, bytes),
                ]
                .into_iter()
                .any(|overflowed| overflowed);
                measurement.queue_peak_bytes = measurement
                    .queue_peak_bytes
                    .max(measurement.queue_current_bytes);
                if scoped_overflowed {
                    measurement.accounting_errors = measurement.accounting_errors.saturating_add(1);
                }
                attributed_scenario_id = scenario_id;
            }
        }
        if scoped_overflowed {
            record_global_accounting_error(site);
            tracing::error!(
                target: "astra::history_work",
                site = site.as_str(),
                bytes,
                "scoped queue accounting saturated; measurement is incomplete"
            );
        }
        trace_operation(site, bytes, 0, 0);
        tracing::debug!(
            target: "astra::history_work",
            site = site.as_str(),
            queue_current_bytes = current,
            queue_peak_bytes = QUEUE_PEAK_BYTES[site as usize].load(Ordering::Relaxed),
            "reserved queued history bytes"
        );
        Self {
            site,
            bytes,
            scenario_id: attributed_scenario_id,
            accounted: true,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

fn try_release_queue_bytes(counter: &AtomicU64, bytes: u64) -> Result<u64, u64> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_sub(bytes)
        })
        .map(|previous| {
            previous
                .checked_sub(bytes)
                .expect("successful checked queue release must produce a value")
        })
}

fn report_queue_release_underflow(site: HistoryWorkSite, bytes: u64, current: u64, scope: &str) {
    record_accounting_error(site);
    tracing::error!(
        target: "astra::history_work",
        site = site.as_str(),
        bytes,
        current,
        scope,
        "queue byte release underflowed; measurement is incomplete"
    );
}

impl Drop for QueueBytesReservation {
    fn drop(&mut self) {
        if !self.accounted {
            return;
        }

        if let Err(current) =
            try_release_queue_bytes(&QUEUE_CURRENT_BYTES[self.site as usize], self.bytes)
        {
            report_queue_release_underflow(self.site, self.bytes, current, "process");
            return;
        }

        if self.scenario_id == 0 {
            return;
        }
        let scenario_underflow = {
            let mut active = lock_active_scenario();
            let Some(active) = active
                .as_mut()
                .filter(|active| active.id == self.scenario_id)
            else {
                return;
            };
            let current = &mut active.sites[self.site as usize].queue_current_bytes;
            match current.checked_sub(self.bytes) {
                Some(remaining) => {
                    *current = remaining;
                    None
                }
                None => Some(*current),
            }
        };
        if let Some(current) = scenario_underflow {
            // Record after releasing ACTIVE_SCENARIO's mutex because error
            // attribution takes the same lock.
            report_queue_release_underflow(self.site, self.bytes, current, "scenario");
        }
    }
}

fn try_reserve_queue_bytes(counter: &AtomicU64, bytes: u64) -> Option<u64> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(bytes)
        })
        .ok()
        .and_then(|previous| previous.checked_add(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serializer;
    use serde_json::json;

    static QUEUE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn serialized_byte_counter_matches_serde_output_across_shapes() {
        for value in [
            json!(null),
            json!("多字节🚀"),
            json!([1, true, {"nested": ["a", "b"]}]),
            json!({"b": 2, "a": {"x": 1}}),
        ] {
            assert_eq!(
                serialized_bytes(&value).unwrap(),
                serde_json::to_vec(&value).unwrap().len() as u64
            );
        }
    }

    #[test]
    fn counting_writer_rejects_overflow_without_changing_count() {
        let mut writer = CountingWriter { bytes: u64::MAX };
        let error = io::Write::write(&mut writer, &[1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(writer.bytes, u64::MAX);
    }

    struct FailingSerialization;

    impl Serialize for FailingSerialization {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(serde::ser::Error::custom("injected serialization failure"))
        }
    }

    #[test]
    fn serialization_measurement_failure_is_accounted() {
        let _test_guard = QUEUE_TEST_LOCK.lock().unwrap();
        let site = HistoryWorkSite::CslFileRewriteSerialization;
        let before = measurement(site).accounting_errors;
        assert_eq!(
            measure_serialized_value_enabled(site, &FailingSerialization),
            None
        );
        assert_eq!(measurement(site).accounting_errors, before + 1);
    }

    #[test]
    fn queue_reservations_track_current_and_peak_bytes() {
        let _test_guard = QUEUE_TEST_LOCK.lock().unwrap();
        let site = HistoryWorkSite::CliPostCommitQueue;
        let baseline = HistoryWorkSnapshot::capture();
        let first = QueueBytesReservation::for_site(site, 13);
        let after_first = HistoryWorkSnapshot::capture();
        let second = QueueBytesReservation::for_site(site, 29);
        let after_second = HistoryWorkSnapshot::capture();

        let baseline_queue = baseline.measurement(site);
        let after_first_queue = after_first.measurement(site);
        let after_second_queue = after_second.measurement(site);
        assert_eq!(
            after_first_queue.queue_current_bytes,
            baseline_queue.queue_current_bytes + 13
        );
        assert_eq!(
            after_second_queue.queue_current_bytes,
            baseline_queue.queue_current_bytes + 42
        );
        assert!(after_second_queue.queue_peak_bytes >= after_second_queue.queue_current_bytes);
        drop(second);
        drop(first);
        assert_eq!(
            HistoryWorkSnapshot::capture()
                .measurement(site)
                .queue_current_bytes,
            baseline_queue.queue_current_bytes
        );
    }

    #[test]
    fn rejected_queue_reservation_records_only_an_accounting_error() {
        struct QueueCurrentRestore {
            site: HistoryWorkSite,
            value: u64,
        }

        impl Drop for QueueCurrentRestore {
            fn drop(&mut self) {
                QUEUE_CURRENT_BYTES[self.site as usize].store(self.value, Ordering::Release);
            }
        }

        let _test_guard = QUEUE_TEST_LOCK.lock().unwrap();
        let site = HistoryWorkSite::CliPostCommitQueue;
        let value = QUEUE_CURRENT_BYTES[site as usize].swap(u64::MAX, Ordering::AcqRel);
        let _restore = QueueCurrentRestore { site, value };
        let before = measurement(site);

        let rejected = QueueBytesReservation::for_site(site, 1);
        let after = measurement(site);

        assert!(!rejected.accounted);
        assert_eq!(after.events, before.events);
        assert_eq!(after.bytes, before.bytes);
        assert_eq!(after.accounting_errors, before.accounting_errors + 1);
        assert_eq!(after.queue_current_bytes, u64::MAX);
        drop(rejected);
        assert_eq!(
            QUEUE_CURRENT_BYTES[site as usize].load(Ordering::Acquire),
            u64::MAX
        );
    }

    #[test]
    fn instrumented_history_work_sites_have_unique_well_formed_metadata() {
        let mut names = std::collections::BTreeSet::new();
        for site in HistoryWorkSite::ALL {
            assert!(names.insert(site.as_str()), "duplicate site name");
            assert!(!site.owner().is_empty());
            assert!(
                (1..=7).contains(&site.primary_target_phase()),
                "{} must name a real primary target phase",
                site.as_str(),
            );
            assert_ne!(site.primary_target_phase_label(), "unassigned");
        }
        assert_eq!(names.len(), HistoryWorkSite::COUNT);
        assert!(HISTORY_WORK_KNOWN_OMISSIONS.is_empty());

        let mut omissions = std::collections::BTreeSet::new();
        for omission in HISTORY_WORK_KNOWN_OMISSIONS {
            assert!(omissions.insert(omission.key), "duplicate omission key");
            assert!(!omission.owner.is_empty());
            assert!((1..=7).contains(&omission.target_phase));
            assert!(!omission.reason.is_empty());
        }
        assert_eq!(omissions.len(), HISTORY_WORK_KNOWN_OMISSIONS.len());
    }

    #[test]
    fn multidimensional_measurement_counts_one_operation() {
        let before = measurement(HistoryWorkSite::CslDatabaseRead);
        record_operation(HistoryWorkSite::CslDatabaseRead, 41, 3, 0);
        let after = measurement(HistoryWorkSite::CslDatabaseRead);

        assert_eq!(after.events - before.events, 1);
        assert_eq!(after.bytes - before.bytes, 41);
        assert_eq!(after.rows - before.rows, 3);
        assert_eq!(after.admission_units - before.admission_units, 0);
    }

    #[test]
    fn atomic_accounting_saturates_or_rejects_without_wrapping() {
        let monotonic = AtomicU64::new(u64::MAX - 1);
        assert!(atomic_saturating_add(&monotonic, 2));
        assert_eq!(monotonic.load(Ordering::Relaxed), u64::MAX);

        let queue = AtomicU64::new(u64::MAX - 1);
        assert_eq!(try_reserve_queue_bytes(&queue, 2), None);
        assert_eq!(
            queue.load(Ordering::Relaxed),
            u64::MAX - 1,
            "a rejected queue reservation must leave current bytes unchanged"
        );

        let queue = AtomicU64::new(5);
        assert_eq!(try_release_queue_bytes(&queue, 6), Err(5));
        assert_eq!(
            queue.load(Ordering::Relaxed),
            5,
            "a rejected queue release must leave current bytes unchanged"
        );
        assert_eq!(try_release_queue_bytes(&queue, 3), Ok(2));
        assert_eq!(queue.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn scoped_counter_saturation_invalidates_the_measurement() {
        let _test_guard = QUEUE_TEST_LOCK.lock().unwrap();
        let site = HistoryWorkSite::CslFileRewriteSerialization;
        let global_errors_before = measurement(site).accounting_errors;
        let scenario = HistoryWorkScenario::begin("scoped-overflow").unwrap();
        {
            let mut active = lock_active_scenario();
            active.as_mut().unwrap().sites[site as usize].bytes = u64::MAX;
        }

        record_scoped_operation(site, 1, 0, 0);
        let report = scenario.finish().unwrap();
        let scoped = report.scoped.measurement(site);

        assert_eq!(scoped.bytes, u64::MAX);
        assert_eq!(scoped.accounting_errors, 1);
        assert_eq!(
            measurement(site).accounting_errors,
            global_errors_before + 1
        );
    }

    #[test]
    fn queue_release_underflow_is_accounted_without_mutating_current_bytes() {
        let _test_guard = QUEUE_TEST_LOCK.lock().unwrap();
        let site = HistoryWorkSite::CslFileRewriteSerialization;
        let before = measurement(site);
        assert_eq!(before.queue_current_bytes, 0);

        drop(QueueBytesReservation {
            site,
            bytes: 1,
            scenario_id: 0,
            accounted: true,
        });

        let after = measurement(site);
        assert_eq!(after.queue_current_bytes, 0);
        assert_eq!(after.accounting_errors, before.accounting_errors + 1);
    }

    #[test]
    fn snapshot_delta_keeps_queue_current_signed() {
        let queue_site = HistoryWorkSite::CliPostCommitQueue;
        let before = HistoryWorkSnapshot {
            sites: HistoryWorkSite::ALL
                .into_iter()
                .map(|site| {
                    (
                        site,
                        HistoryWorkMeasurement {
                            queue_current_bytes: if site == queue_site { 19 } else { 0 },
                            queue_peak_bytes: if site == queue_site { 31 } else { 0 },
                            ..HistoryWorkMeasurement::default()
                        },
                    )
                })
                .collect(),
        };
        let after = HistoryWorkSnapshot {
            sites: HistoryWorkSite::ALL
                .into_iter()
                .map(|site| {
                    (
                        site,
                        HistoryWorkMeasurement {
                            events: if site == queue_site { 2 } else { 0 },
                            queue_current_bytes: if site == queue_site { 7 } else { 0 },
                            queue_peak_bytes: if site == queue_site { 43 } else { 0 },
                            ..HistoryWorkMeasurement::default()
                        },
                    )
                })
                .collect(),
        };

        let delta = after.delta_since(&before).measurement(queue_site);
        assert_eq!(delta.events, 2);
        assert_eq!(delta.queue_current_bytes_change, -12);
        assert_eq!(delta.queue_peak_bytes_increase, 12);
    }

    #[test]
    fn dedicated_scenario_has_interval_local_queue_peak_independent_of_process_peak() {
        let _test_guard = QUEUE_TEST_LOCK.lock().unwrap();
        let site = HistoryWorkSite::CliPostCommitQueue;
        // Establish an earlier process peak that is deliberately higher than
        // the scenario peak.
        drop(QueueBytesReservation::for_site(site, 101));

        let scenario = HistoryWorkScenario::begin("queue-local-peak").unwrap();
        let first = QueueBytesReservation::for_site(site, 11);
        let second = QueueBytesReservation::for_site(site, 23);
        drop(second);
        drop(first);
        let report = scenario.finish().unwrap();
        let scoped = report.scoped.measurement(site);

        assert_eq!(scoped.events, 2);
        assert_eq!(scoped.bytes, 34);
        assert_eq!(scoped.queue_current_bytes, 0);
        assert_eq!(scoped.queue_peak_bytes, 34);
        assert_eq!(
            report
                .global_delta
                .measurement(site)
                .queue_peak_bytes_increase,
            0,
            "the dedicated scenario peak must not depend on a new process peak"
        );
    }

    #[test]
    fn overlapping_scenarios_are_rejected_and_drop_releases_slot() {
        let _test_guard = QUEUE_TEST_LOCK.lock().unwrap();
        let first = HistoryWorkScenario::begin("first").unwrap();
        let error = HistoryWorkScenario::begin("second").unwrap_err();
        assert!(matches!(
            error,
            HistoryWorkScenarioError::AlreadyActive { label, .. } if label == "first"
        ));
        drop(first);

        HistoryWorkScenario::begin("after-abort")
            .unwrap()
            .finish()
            .unwrap();
    }
}
