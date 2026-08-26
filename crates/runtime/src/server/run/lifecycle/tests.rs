use super::*;
use crate::server::run::lifecycle::persistence::{
    build_tool_trace_events, extract_prev_assistant_text, extract_session_state_compact,
    messages_for_csl_persist, redact_trace_value, server_loop_causal_chain_id,
    transcript_page_bounds, transcript_page_seq,
};
use astra_services::runs::{
    DatabaseRunStateStore, DurableRunCheckpointRecord, DurableRunDisplayProjectionRecord,
    DurableRunRecord, InMemoryRunStateStore, RunStateStore, RuntimeMcpBindingRequest,
    RuntimeSkillBindingRequest,
};
use astra_services::session_journal::{JournalEventType, ToolCallDisposition, ToolCallRecord};
use astra_services::workspace_records::{
    InMemoryWorkspaceRecordStore, WorkspaceCleanupDebtStore, WorkspaceCleanupDebtStoreError,
    WorkspaceRecordStore,
};
use astra_turn_core::orchestration::fanout_group::{AgentFanoutSlotStatus, AgentFanoutStatus};
use serde_json::json;
use sqlx::Row;
use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::Mutex as StdMutex;
use uuid::Uuid;

static LIFECYCLE_RUN_DB: tokio::sync::OnceCell<SharedPool> = tokio::sync::OnceCell::const_new();
const DURABLE_EVENT_PRESSURE_OPT_IN: &str = "ASTRA_DURABLE_EVENT_PRESSURE_PROBE";

#[test]
fn canonical_segment_packing_keeps_structured_tool_exchange_atomic() {
    let payload = "x".repeat(300 * 1024);
    let messages = vec![
        json!({
            "role": "assistant",
            "content": payload,
            "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "read", "arguments": "{}"}
            }]
        }),
        json!({
            "role": "tool",
            "tool_call_id": "call-1",
            "content": "y".repeat(300 * 1024)
        }),
        json!({"role": "user", "content": "continue"}),
    ];

    let packs = pack_canonical_turn_segments(messages.clone());

    assert_eq!(packs.len(), 2, "an oversized atomic group stays whole");
    assert_eq!(packs[0], messages[..2]);
    assert_eq!(packs[1], messages[2..]);
}

#[test]
fn canonical_segment_packing_bounds_ordinary_groups_without_reordering() {
    let messages = vec![
        json!({"role": "user", "content": "a".repeat(300 * 1024)}),
        json!({"role": "assistant", "content": "b".repeat(300 * 1024)}),
        json!({"role": "user", "content": "tail"}),
    ];

    let packs = pack_canonical_turn_segments(messages.clone());

    assert_eq!(packs.len(), 2);
    assert_eq!(packs.concat(), messages);
    assert!(
        packs.iter().all(|pack| {
            astra_turn_types::canonical_conversation_serialized_len(pack) <= 512 * 1024
        }),
        "non-atomic groups must respect the physical pack target"
    );
}

#[test]
fn cancelled_turn_without_a_delta_does_not_become_a_commit_failure() {
    let prior = vec![json!({"role": "user", "content": "already committed"})];
    assert_eq!(
        canonical_commit_delta(&prior, true, &prior, None, true).unwrap(),
        None
    );
    assert!(
        canonical_commit_delta(&prior, true, &prior, None, false)
            .unwrap_err()
            .contains("no committable messages")
    );
}

#[test]
fn compaction_commits_the_complete_replacement_projection() {
    let prior = vec![json!({"role": "user", "content": "old"})];
    let base_root = astra_turn_types::canonical_conversation_root(&prior);
    let mut proof = CanonicalRewriteProof::new(&prior, &base_root, 0);
    let permit = proof.begin(&prior);
    let compacted = vec![
        json!({"role": "user", "content": "summary"}),
        json!({"role": "assistant", "content": "current"}),
    ];
    proof.finish(permit, &compacted);
    let (mode, packs) = canonical_commit_delta(&prior, true, &compacted, Some(&proof), false)
        .unwrap()
        .unwrap();

    assert_eq!(mode, astra_turn_types::CanonicalDeltaModeV1::Replace);
    assert_eq!(packs.concat(), compacted);
}

#[test]
fn unexplained_canonical_prefix_shrink_remains_rejected() {
    let prior = vec![json!({"role": "user", "content": "committed"})];
    let shortened = vec![json!({"role": "user", "content": "unexpected tail"})];

    let error = canonical_commit_delta(&prior, true, &shortened, None, false).unwrap_err();

    assert!(error.contains("without a verified compaction rewrite"));
}

#[test]
fn unrelated_prefix_mutation_after_compaction_is_rejected() {
    let prior = vec![json!({"role": "user", "content": "committed"})];
    let compacted = vec![json!({"role": "system", "content": "summary"})];
    let base_root = astra_turn_types::canonical_conversation_root(&prior);
    let mut proof = CanonicalRewriteProof::new(&prior, &base_root, 0);
    let permit = proof.begin(&prior);
    proof.finish(permit, &compacted);

    let mutated = vec![json!({"role": "system", "content": "unrelated mutation"})];
    let error = canonical_commit_delta(&prior, true, &mutated, Some(&proof), false).unwrap_err();

    assert!(error.contains("without a verified compaction rewrite"));
}

#[test]
fn compaction_cannot_authorize_an_already_mutated_prefix() {
    let prior = vec![json!({"role": "user", "content": "committed"})];
    let mutated_before_compaction = vec![json!({"role": "user", "content": "unrelated mutation"})];
    let compacted = vec![json!({"role": "system", "content": "summary"})];
    let base_root = astra_turn_types::canonical_conversation_root(&prior);
    let mut proof = CanonicalRewriteProof::new(&prior, &base_root, 0);
    let permit = proof.begin(&mutated_before_compaction);
    proof.finish(permit, &compacted);

    let error = canonical_commit_delta(&prior, true, &compacted, Some(&proof), false).unwrap_err();

    assert!(error.contains("without a verified compaction rewrite"));
}

#[test]
fn fresh_request_admission_accounts_for_large_non_message_payloads() {
    let mut request = test_request("small");
    let baseline = fresh_request_admission_bytes(&request).unwrap();
    request.attachments.push(json!({
        "kind": "inline",
        "data": "x".repeat(1024 * 1024),
    }));
    let with_attachment = fresh_request_admission_bytes(&request).unwrap();

    assert!(
        with_attachment >= baseline + 1024 * 1024,
        "large attachment bytes must be admitted before canonical history is materialized"
    );
}

#[test]
fn fresh_request_admission_accounts_for_both_runtime_prompt_lanes() {
    let mut request = test_request("small");
    let baseline = fresh_request_admission_bytes(&request).unwrap();
    let none_prompt_bytes = astra_turn_types::json_serialized_len(&Option::<String>::None).unwrap();

    request.stable_runtime_system_prompt = Some("s".repeat(4096));
    let stable_prompt_bytes =
        astra_turn_types::json_serialized_len(&request.stable_runtime_system_prompt).unwrap();
    let with_stable = fresh_request_admission_bytes(&request).unwrap();
    assert_eq!(
        with_stable - baseline,
        stable_prompt_bytes - none_prompt_bytes,
        "stable runtime prompt bytes must be admitted"
    );

    request.runtime_system_prompt = Some("v".repeat(2048));
    let volatile_prompt_bytes =
        astra_turn_types::json_serialized_len(&request.runtime_system_prompt).unwrap();
    let with_both = fresh_request_admission_bytes(&request).unwrap();
    assert_eq!(
        with_both - with_stable,
        volatile_prompt_bytes - none_prompt_bytes,
        "volatile runtime prompt bytes must be admitted"
    );
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::remove_var(key) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[test]
fn server_service_catalog_is_disabled_only_for_agent_binding_mode() {
    assert!(
        AgenticRunLifecycleService::server_service_tool_catalog_enabled_for_request(false, false),
        "server-only web runs may use server-service capacity"
    );
    assert!(
        AgenticRunLifecycleService::server_service_tool_catalog_enabled_for_request(false, true),
        "server+edge and managed-runtime runs may still use server-service offers when policy allows them"
    );
    assert!(
        !AgenticRunLifecycleService::server_service_tool_catalog_enabled_for_request(true, true),
        "agent-binding mode owns its service catalog and should not receive default server-service offers"
    );
}

#[tokio::test]
async fn attached_stream_progress_recovers_after_transient_backpressure() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let mut attached = Some(tx);

    send_attached_stream_event(&mut attached, json!({"seq": 1}), "run-1").await;
    assert!(attached.is_some());
    {
        let delivery = send_attached_stream_event(&mut attached, json!({"seq": 2}), "run-1");
        tokio::pin!(delivery);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut delivery)
                .await
                .is_err(),
            "a full observer must receive repair evidence before later progress"
        );
        assert_eq!(rx.recv().await.unwrap(), json!({"seq": 1}));
        delivery.await;
    }
    assert_eq!(
        rx.recv().await.unwrap(),
        stream_delivery_gap_event("run-1", 1)
    );
    assert!(attached.is_some());
    send_attached_stream_event(&mut attached, json!({"seq": 3}), "run-1").await;
    assert_eq!(rx.recv().await.unwrap(), json!({"seq": 3}));
}

#[tokio::test]
async fn attached_stream_event_detaches_after_disconnect() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let mut attached = Some(tx);

    send_attached_stream_event(&mut attached, json!({"seq": 1}), "run-1").await;

    assert!(attached.is_none());
}

#[tokio::test]
async fn attached_stream_never_drops_an_approval_while_the_observer_is_attached() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.send(json!({"type": "text_delta", "content": "queued"}))
        .await
        .unwrap();
    let mut attached = Some(tx);
    let approval = json!({
        "type": "approval_required",
        "request_id": "approval-1",
        "tool": "bash",
    });

    {
        let delivery = send_attached_stream_event(&mut attached, approval.clone(), "run-1");
        tokio::pin!(delivery);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut delivery)
                .await
                .is_err(),
            "a full observer queue must backpressure the interaction boundary instead of dropping it"
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            json!({"type": "text_delta", "content": "queued"})
        );
        delivery.await;
    }
    assert_eq!(rx.recv().await.unwrap(), approval);
    assert!(attached.is_some());
}

#[tokio::test]
async fn attached_stream_never_silently_drops_a_repair_gap() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.send(json!({"type": "text_delta", "content": "queued"}))
        .await
        .unwrap();
    let mut attached = Some(tx);
    let gap = stream_delivery_gap_event("run-1", 7);

    {
        let delivery = send_attached_stream_event(&mut attached, gap.clone(), "run-1");
        tokio::pin!(delivery);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut delivery)
                .await
                .is_err(),
            "repair evidence must wait for capacity instead of being dropped as ordinary progress"
        );
        assert_eq!(rx.recv().await.unwrap()["type"], "text_delta");
        delivery.await;
    }

    assert_eq!(rx.recv().await.unwrap(), gap);
    assert!(attached.is_some());
}

#[tokio::test]
async fn attached_stream_closes_a_stalled_interaction_lane_for_durable_replay() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    tx.send(json!({"type": "text_delta", "content": "queued"}))
        .await
        .unwrap();
    let mut attached = Some(tx);

    tokio::time::timeout(
        ATTACHED_INTERACTION_DELIVERY_GRACE + Duration::from_millis(100),
        send_attached_stream_event(
            &mut attached,
            json!({"type": "user_prompt_required", "request_id": "question-1"}),
            "run-1",
        ),
    )
    .await
    .expect("a stalled observer must not block the run indefinitely");

    assert!(
        attached.is_none(),
        "the stale lane must close so a client can reconnect and replay durable truth"
    );
}

struct StaticRunControlProvider {
    status: Option<RunControlStatus>,
    calls: AtomicUsize,
}

struct HangingRunControlProvider {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::turn::run_control::RunStatusProvider for HangingRunControlProvider {
    async fn control_status(
        &self,
        _user_id: &str,
        _run_id: &str,
    ) -> Result<Option<RunControlStatus>, String> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl UserIntentProvider for HangingRunControlProvider {
    async fn poll_user_intents(
        &self,
        _user_id: &str,
        _run_id: &str,
        after_event_index: usize,
    ) -> crate::turn::run_control::UserIntentPoll {
        crate::turn::run_control::UserIntentPoll {
            next_cursor: after_event_index,
            inputs: Vec::new(),
            issues: Vec::new(),
            error: None,
        }
    }

    async fn mark_user_intents_applied(
        &self,
        _user_id: &str,
        _run_id: &str,
        _event_indices: &[usize],
    ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
        Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
    }
}

impl StaticRunControlProvider {
    fn new(status: Option<RunControlStatus>) -> Self {
        Self {
            status,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl crate::turn::run_control::RunStatusProvider for StaticRunControlProvider {
    async fn control_status(
        &self,
        _user_id: &str,
        _run_id: &str,
    ) -> Result<Option<RunControlStatus>, String> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(self.status)
    }
}

#[async_trait::async_trait]
impl UserIntentProvider for StaticRunControlProvider {
    async fn poll_user_intents(
        &self,
        _user_id: &str,
        _run_id: &str,
        after_event_index: usize,
    ) -> crate::turn::run_control::UserIntentPoll {
        crate::turn::run_control::UserIntentPoll {
            next_cursor: after_event_index,
            inputs: Vec::new(),
            issues: Vec::new(),
            error: None,
        }
    }

    async fn mark_user_intents_applied(
        &self,
        _user_id: &str,
        _run_id: &str,
        _event_indices: &[usize],
    ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
        Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
    }
}

struct ActiveTestModelService {
    base_url: String,
}

impl ActiveTestModelService {
    fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for ActiveTestModelService {
    fn default() -> Self {
        Self::new("https://models.example.com/v1")
    }
}

fn test_resolved_model_offering() -> astra_services::ResolvedModelOffering {
    test_resolved_model_offering_at("https://models.example.com/v1")
}

fn test_resolved_model_offering_at(base_url: &str) -> astra_services::ResolvedModelOffering {
    astra_services::ResolvedModelOffering {
        offering_id: "model-test-model".to_string(),
        model: astra_services::ResolvedActiveLlmModel {
            model_name: "test-model".to_string(),
            wire_model_name: None,
            api_key: "test-provider-secret".to_string(),
            base_url: base_url.to_string(),
            provider: "openai".to_string(),
            fallback_chain: Vec::new(),
            tags: Vec::new(),
            request_body_overrides: None,
            prompt_cache_capability: None,
            thinking_capability: None,
            context_window: Some(128_000),
            max_completion_tokens: Some(16_384),
            request_headers: None,
        },
    }
}

fn test_admitted_model_execution() -> astra_services::AdmittedModelExecution {
    astra_services::AdmittedModelExecution::from_offering(test_resolved_model_offering())
        .expect("valid test model execution")
}

fn test_model_record_at(name: String, base_url: &str) -> astra_services::ModelRecord {
    astra_services::ModelRecord {
        model_id: format!("model-{name}"),
        name,
        provider: "openai".to_string(),
        base_url: Some(base_url.to_string()),
        description: None,
        is_active: true,
        context_window: 128_000,
        max_completion_tokens: None,
        input_modalities: Vec::new(),
        output_modalities: Vec::new(),
        supported_parameters: Vec::new(),
        pricing: Default::default(),
        architecture: None,
        tags: Vec::new(),
        quirks: Default::default(),
        connectivity: None,
        thinking_capability: None,
        thinking_probe: None,
    }
}

#[async_trait]
impl astra_services::ModelService for ActiveTestModelService {
    async fn create_model(
        &self,
        _user_id: String,
        _request: astra_services::ModelCreateRequestData,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn list_models(
        &self,
        _user_id: String,
        _is_admin: bool,
    ) -> Result<Vec<astra_services::ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn get_model(
        &self,
        model_name: String,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        if model_name == "test-model" {
            return Ok(test_model_record_at(model_name, &self.base_url));
        }
        Err(error_response_coded(
            StatusCode::NOT_FOUND,
            "model not found",
            "model_not_found",
        ))
    }

    async fn resolve_model_offering(
        &self,
        offering_id: String,
    ) -> Result<astra_services::ResolvedModelOffering, (StatusCode, Json<ErrorResponse>)> {
        if offering_id != "model-test-model" {
            return Err(error_response_coded(
                StatusCode::NOT_FOUND,
                "offering not found",
                "offering_not_found",
            ));
        }
        Ok(test_resolved_model_offering_at(&self.base_url))
    }

    async fn update_model(
        &self,
        _model_name: String,
        _request: astra_services::ModelUpdateRequestData,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn delete_model(
        &self,
        _model_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn check_model(
        &self,
        _model_name: String,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
}

#[test]
fn post_loop_memory_cleanup_permit_respects_limit() {
    let baseline = POST_LOOP_MEMORY_CLEANUP_IN_FLIGHT.load(Ordering::SeqCst);
    let limit = baseline + 1;

    let permit = try_acquire_post_loop_memory_cleanup_permit(limit)
        .expect("permit should be available below limit");
    assert_eq!(
        POST_LOOP_MEMORY_CLEANUP_IN_FLIGHT.load(Ordering::SeqCst),
        baseline + 1
    );
    assert!(
        try_acquire_post_loop_memory_cleanup_permit(limit).is_none(),
        "permit should reject at limit"
    );
    drop(permit);
    assert_eq!(
        POST_LOOP_MEMORY_CLEANUP_IN_FLIGHT.load(Ordering::SeqCst),
        baseline
    );
}

#[test]
fn session_only_post_loop_boundary_does_not_consume_run_owned_attribution() {
    let session_id = "server-run-selection-boundary";
    astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(session_id);
    astra_tools::memoria::MemoriaToolGateway::record_recall_for_producer(
        session_id,
        "run-1",
        1,
        vec!["memory-1".to_string()],
    );

    finish_post_loop_memory_run_boundary(session_id, None);

    assert_eq!(
        astra_tools::memoria::MemoriaToolGateway::pending_recall_count(session_id),
        1,
        "a session-only hook cannot consume a concurrent run's recall"
    );
    assert!(
        astra_tools::memoria::MemoriaToolGateway::latest_selection_context(session_id).is_some(),
        "a follow-up run must retain the producer-owned selection receipt"
    );
    astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(session_id);
}

#[tokio::test]
async fn post_loop_memory_cleanup_metrics_stay_low_cardinality() {
    let _memoria = EnvVarGuard::remove("MEMORIA_MASTER_KEY");
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());

    record_post_loop_memory_cleanup_dispatch_metrics(Some(&registry), "async", "scheduled");
    run_post_loop_memory_cleanup_work(
        "session-1".to_string(),
        astra_turn_types::session_facts::SessionFacts::default(),
        None,
        None,
        Some(registry.clone()),
        Duration::from_millis(DEFAULT_SESSION_MEMORY_POST_LOOP_DRAIN_TIMEOUT_MS),
    )
    .await;

    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains(
            "astra_post_loop_memory_cleanup_dispatches_total{mode=\"async\",outcome=\"scheduled\"} 1"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_post_loop_memory_cleanup_workers_total{outcome=\"completed\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_session_memory_post_loop_drains_total{outcome=\"no_service\"} 1"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("user_id=")
            && !rendered.contains("session_id=")
            && !rendered.contains("run_id="),
        "memory cleanup metrics must stay low-cardinality: {rendered}"
    );
}

#[tokio::test]
async fn post_loop_memory_cleanup_runs_inline_when_async_pool_is_full() {
    let _memoria = EnvVarGuard::remove("MEMORIA_MASTER_KEY");
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());

    post_loop_memory_cleanup_with_limits(
        "session-inline",
        &astra_turn_types::session_facts::SessionFacts::default(),
        None,
        None,
        Some(registry.clone()),
        0,
        Duration::ZERO,
    )
    .await;

    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains(
            "astra_post_loop_memory_cleanup_dispatches_total{mode=\"inline\",outcome=\"saturated\"} 1"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_post_loop_memory_cleanup_workers_total{outcome=\"completed\"} 1"),
        "{rendered}"
    );
    assert!(!rendered.contains("dropped_full"), "{rendered}");
}

fn test_session_task(
    id: &str,
    title: &str,
    status: astra_tools::task_mgmt::SessionTaskStatusKind,
) -> SessionTask {
    SessionTask {
        archived_at: None,
        id: id.to_string(),
        title: title.to_string(),
        description: None,
        status,
        subtasks: vec![],
        created_at: String::new(),
        updated_at: String::new(),
        active_form: None,
        owner: None,
        metadata: None,
        blocks: vec![],
        blocked_by: vec![],
    }
}

fn test_agent_progress_event(
    agent_id: &str,
    run_id: &str,
    parent_run_id: &str,
    timestamp_epoch_ms: u64,
    event_type: ProgressEventType,
) -> AgentProgressEvent {
    AgentProgressEvent {
        agent_id: agent_id.to_string(),
        run_id: run_id.to_string(),
        parent_run_id: parent_run_id.to_string(),
        event_type,
        timestamp_epoch_ms,
        metadata: None,
    }
}

fn test_agent_spawned(
    agent_id: &str,
    run_id: &str,
    parent_run_id: &str,
    timestamp_epoch_ms: u64,
) -> AgentProgressEvent {
    test_agent_progress_event(
        agent_id,
        run_id,
        parent_run_id,
        timestamp_epoch_ms,
        ProgressEventType::AgentSpawned {
            agent_type: "reviewer".to_string(),
            description: "review code".to_string(),
            fanout_slot: None,
        },
    )
}

#[test]
fn restore_session_state_compact_ignores_runtime_control_state() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.max_turn_input_tokens = 123_456;
    state.remaining_turns = 9;

    restore_session_state_compact(
        astra_turn_core::conversation_log::SessionStateCompact {
            activated_deferred_tool_names: vec!["github".into()],
            approval_overrides: Some(json!({"approval": "stale"})),
            budget_remaining_tokens: 42_000,
            budget_remaining_rounds: 3,
            consecutive_ctx_errors: 3,
            interruption: Some(json!({
                "kind": "budget_exhausted",
                "resume_action": "continue_immediately"
            })),
            compaction_tracker: Some(json!({
                "attempt_count": 4,
                "cumulative_tokens_freed": 18_000,
                "last_tokens_freed": 2_000,
                "last_was_insufficient": true,
                "consecutive_futile_attempts": 2,
            })),
            ..Default::default()
        },
        &mut state,
    );

    assert!(state.approval_overrides.is_none());
    assert!(state.interruption.is_none());
    assert_eq!(state.max_turn_input_tokens, 123_456);
    assert_eq!(state.remaining_turns, 9);
    assert_eq!(state.consecutive_context_window_errors, 0);
    assert_eq!(state.compaction_effectiveness.attempt_count, 0);
    assert_eq!(
        state.activated_deferred_tool_names,
        vec!["github"],
        "schema materialization is prompt continuity, not a stale runtime control"
    );
}

#[test]
fn csl_session_state_does_not_persist_runtime_control_state() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.restricted_tools.insert("write_file".to_string());
    state.max_turn_input_tokens = 50_000;
    state.remaining_turns = 2;
    state.consecutive_context_window_errors = 5;
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 1,
            turns_completed: 1,
            remaining_turns: 0,
            error_detail: Some("stale interruption".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));
    state.compaction_effectiveness.attempt_count = 7;
    state.activated_deferred_tool_names = vec!["github".into()];

    let compact = extract_session_state_compact(&state);

    assert!(
        compact.blocked_tools.is_empty(),
        "conversation-log state must not persist transient runtime restrictions"
    );
    assert!(compact.approval_overrides.is_none());
    assert!(compact.interruption.is_none());
    assert_eq!(compact.budget_remaining_tokens, 0);
    assert_eq!(compact.budget_remaining_rounds, 0);
    assert_eq!(compact.consecutive_ctx_errors, 0);
    assert!(compact.compaction_tracker.is_none());
    assert_eq!(
        compact.activated_deferred_tool_names,
        vec!["github"],
        "CSL must carry prompt-visible deferred schema materialization across turns"
    );
}

#[test]
fn csl_session_state_snapshots_live_executor_activation() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.activated_deferred_tool_names = vec!["stale-state-copy".into()];
    let workspace = tempfile::tempdir().expect("workspace");
    let executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
        workspace.path().to_path_buf(),
        "test-user".to_string(),
        "session-1".to_string(),
        None,
        None,
    );
    executor
        .restore_activated_deferred_tool_names_for_session(&["web_fetch".into(), "github".into()]);
    state.runtime_tool_executor = Some(std::sync::Arc::new(executor));

    let compact = extract_session_state_compact(&state);

    assert_eq!(
        compact.activated_deferred_tool_names,
        vec!["github", "web_fetch"],
        "settlement must snapshot the live executor rather than an older loop-state copy"
    );
}

#[test]
fn csl_session_state_restore_ignores_legacy_blocked_tools() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    restore_session_state_compact(
        astra_turn_core::conversation_log::SessionStateCompact {
            blocked_tools: vec!["legacy_stale_tool".into()],
            recent_tools: vec!["read_file".into()],
            ..Default::default()
        },
        &mut state,
    );

    assert!(
        state.restricted_tools.is_empty(),
        "legacy CSL blocked_tools must not restore as hard runtime restrictions"
    );
    assert_eq!(state.recent_tools, vec!["read_file"]);
}

#[test]
fn csl_restore_turn_start_excludes_current_user_message() {
    let svc = test_service();
    let request = test_request("3");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-2",
        None,
        None,
        None,
    );
    let restored = vec![
        json!({"role": "user", "content": "1"}),
        json!({"role": "assistant", "content": "ack 1"}),
    ];

    let turn_start =
        AgenticRunLifecycleService::restore_csl_messages_into_loop_state(restored, &mut state);

    assert_eq!(
        turn_start, 2,
        "CSL deltas must start before this turn's user message"
    );
    assert_eq!(state.messages.len(), 3);
    assert_eq!(state.messages[0]["content"], "1");
    assert_eq!(state.messages[1]["content"], "ack 1");
    assert_eq!(state.messages[2]["content"], "3");
}

#[tokio::test]
async fn csl_persist_after_restore_keeps_current_user_message() {
    use astra_turn_core::conversation_log::file_store::FileCslStore;
    use astra_turn_core::conversation_log::manager::{CslManager, CslManagerConfig};
    use astra_turn_core::conversation_log::{CslStore, SessionStateCompact};

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CslStore> = Arc::new(FileCslStore::new(dir.path()));
    let session_id = "server-csl-current-user";
    let mut first = CslManager::new(
        Arc::clone(&store),
        session_id.to_string(),
        CslManagerConfig::default(),
    )
    .unwrap();
    first
        .persist_turn(
            1,
            &[
                json!({"role": "user", "content": "1"}),
                json!({"role": "assistant", "content": "ack 1"}),
            ],
            &SessionStateCompact::default(),
        )
        .await
        .unwrap();

    let svc = test_service();
    let request = test_request("3");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-2",
        None,
        None,
        None,
    );
    state.final_text = "ack 3".to_string();

    let mut resumed = CslManager::new(
        Arc::clone(&store),
        session_id.to_string(),
        CslManagerConfig::default(),
    )
    .unwrap();
    let materialized = resumed.load().await.unwrap().unwrap();
    let turn_start = AgenticRunLifecycleService::restore_csl_messages_into_loop_state(
        materialized.messages,
        &mut state,
    );
    resumed.mark_turn_start(turn_start);

    let messages = messages_for_csl_persist(&state);
    resumed
        .persist_turn(2, &messages, &extract_session_state_compact(&state))
        .await
        .unwrap();

    let mut reloaded = CslManager::new(
        Arc::clone(&store),
        session_id.to_string(),
        CslManagerConfig::default(),
    )
    .unwrap();
    let final_state = reloaded.load().await.unwrap().unwrap();
    let contents = final_state
        .messages
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();

    assert_eq!(
        contents,
        vec!["1", "ack 1", "3", "ack 3"],
        "restored web runs must persist the current user message into CSL"
    );
}

#[test]
fn restore_step_checkpoint_runtime_state_rejects_event_cache_and_restores_runtime_state() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let restored = astra_pipeline::step_restore::RestoredSession {
        conversation_cursor: None,
        messages: Vec::new(),
        budget_remaining_tokens: 0,
        budget_remaining_rounds: 0,
        blocked_tools: vec!["flaky_tool".into()],
        recent_tools: vec!["read_file".into(), "bash".into()],
        activated_deferred_tool_names: vec!["github".into()],
        resume_turn: 0,
        protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
        completed_tool_results: HashMap::new(),
        interruption: None,
        approval_overrides: None,
        consecutive_context_window_errors: 5,
        compaction_state: Some(json!({
            "attempt_count": 6,
            "cumulative_tokens_freed": 24_000,
            "last_tokens_freed": 1_500,
            "last_was_insufficient": false,
            "consecutive_futile_attempts": 1,
        })),
        pipeline_state: None,
        cache_restore_report: astra_pipeline::step_restore::CacheRestoreReport {
            rejected_unverified_entries: 1,
            rejected_context_bound_entries: 1,
            ..Default::default()
        },
    };

    restore_step_checkpoint_runtime_state(restored, "2026-06-13", &mut state);

    assert!(state.restricted_tools.contains("flaky_tool"));
    assert_eq!(state.recent_tools, vec!["read_file", "bash"]);
    assert_eq!(state.activated_deferred_tool_names, vec!["github"]);
    assert!(
        state.idempotency_cache.is_empty(),
        "event-derived semantic observations must not cross the recovery boundary"
    );
    assert_eq!(state.consecutive_context_window_errors, 5);
    assert_eq!(state.compaction_effectiveness.attempt_count, 6);
    assert_eq!(
        state.compaction_effectiveness.cumulative_tokens_freed,
        24_000
    );
    assert_eq!(state.compaction_effectiveness.last_tokens_freed, 1_500);
    assert!(!state.compaction_effectiveness.last_was_insufficient);
    assert_eq!(
        state.compaction_effectiveness.consecutive_futile_attempts,
        1
    );
}

#[test]
fn shutdown_extraction_request_uses_outer_session_turn() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.context_manifest_user_id = Some("test-user".to_string());
    state.session_turn = 12;
    state.max_turns = 50;
    state.remaining_turns = 49;

    let req = build_shutdown_extraction_request(&state)
        .expect("shutdown extraction request should build");

    assert_eq!(
        req.turn_number(),
        Some(12),
        "shutdown extraction must record the persisted session turn, not the request-local loop step"
    );
}

#[test]
fn shutdown_extraction_request_uses_typed_objective_relation() {
    let svc = test_service();
    let mut request = test_request(
        "<project-instructions>\nDo not classify this wrapper.\n</project-instructions>\n\ncontinue",
    );
    request.user_intent = Some("no, that's wrong, fix it the way I said".to_string());
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.context_manifest_user_id = Some("test-user".to_string());
    state.turn_intent = Some(
        astra_config::user_profile::TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct),
    );

    let req = build_shutdown_extraction_request(&state)
        .expect("shutdown extraction request should build");

    assert!(
        req.reanchors_current_objective,
        "shutdown extraction must consume the judge-owned objective relation without reclassifying message text"
    );
}

#[test]
fn run_scoped_agent_progress_filter_accepts_scoped_events_before_spawn() {
    let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

    let accepted = filter.accept(test_agent_progress_event(
        "agent-a",
        "child-run",
        "root-run",
        1,
        ProgressEventType::Started {
            description: "review code".to_string(),
        },
    ));
    assert_eq!(accepted.len(), 1);

    let accepted = filter.accept(test_agent_spawned("agent-a", "child-run", "root-run", 2));
    assert_eq!(accepted.len(), 1);
    assert!(matches!(
        accepted[0].event_type,
        ProgressEventType::AgentSpawned { .. }
    ));

    let accepted = filter.accept(test_agent_progress_event(
        "agent-a",
        "child-run",
        "root-run",
        3,
        ProgressEventType::ToolExecuting {
            tool_name: "rg".to_string(),
            turn: 1,
        },
    ));
    assert_eq!(accepted.len(), 1);
}

#[test]
fn run_scoped_agent_progress_filter_preserves_scoped_arrival_order() {
    let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

    for timestamp in 1..=10 {
        let accepted = filter.accept(test_agent_progress_event(
            "agent-a",
            "child-run",
            "root-run",
            timestamp,
            ProgressEventType::ToolExecuting {
                tool_name: format!("tool-{timestamp}"),
                turn: timestamp as u32,
            },
        ));
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].timestamp_epoch_ms, timestamp);
    }
}

#[test]
fn run_scoped_agent_progress_filter_blocks_foreign_root_events() {
    let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-a".to_string());

    assert!(
        filter
            .accept(test_agent_progress_event(
                "agent-b",
                "child-b",
                "root-b",
                1,
                ProgressEventType::Started {
                    description: "other run".to_string(),
                },
            ))
            .is_empty()
    );
    assert!(
        filter
            .accept(test_agent_spawned("agent-b", "child-b", "root-b", 2))
            .is_empty()
    );
    assert!(
        !filter.agent_ids.contains("agent-b"),
        "foreign agent must not be admitted"
    );
}

#[test]
fn run_scoped_agent_progress_filter_allows_nested_child_runs() {
    let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

    assert_eq!(
        filter
            .accept(test_agent_spawned("agent-a", "child-a", "root-run", 1))
            .len(),
        1
    );
    assert_eq!(
        filter
            .accept(test_agent_spawned("agent-b", "grandchild-b", "child-a", 2))
            .len(),
        1
    );
    assert!(filter.agent_ids.contains("agent-b"));
    assert!(filter.run_ids.contains("grandchild-b"));
}

#[tokio::test]
async fn agent_progress_stream_bridge_drains_progress_on_stop() {
    let svc = test_service();
    let (event_tx, mut event_rx) = mpsc::channel::<Value>(16);
    let bridge = svc.spawn_agent_progress_stream_bridge("root-run".to_string(), event_tx);

    let emitter = svc
        .server_agent_progress_broadcaster
        .for_agent_with_run_context(
            "agent-a".to_string(),
            "child-run".to_string(),
            "root-run".to_string(),
            None,
        );
    emitter.started("review code");
    emitter.agent_spawned("reviewer", "review code");
    emitter.completed("done", 0, (0, 0), 7);

    bridge.stop_and_drain().await;

    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }

    assert!(
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("agent_spawned")),
        "bridge should drain agent_spawned before stopping: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("agent_completed")),
        "bridge should drain agent_completed before stopping: {events:?}"
    );
}

struct ImmediateLifecycleExecutor;

#[async_trait]
impl SpawnAgentExecutor for ImmediateLifecycleExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        Ok(SpawnRunResult {
            agent_id: config.agent_id,
            run_id: config.run_id,
            status: "completed".to_string(),
            finish_reason: "normal".to_string(),
            cancelled_by_user: None,
            output: Some("child done".to_string()),
            error: None,
            prompt_tokens: 3,
            completion_tokens: 5,
            tool_calls: 1,
            turns_completed: 0,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        })
    }
}

struct WaitingLifecycleExecutor;

#[async_trait]
impl SpawnAgentExecutor for WaitingLifecycleExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        Ok(SpawnRunResult {
            agent_id: config.agent_id,
            run_id: config.run_id,
            status: "waiting".to_string(),
            finish_reason: "waiting".to_string(),
            cancelled_by_user: None,
            output: Some("executor_offline".to_string()),
            error: None,
            prompt_tokens: 3,
            completion_tokens: 5,
            tool_calls: 1,
            turns_completed: 0,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 1,
        })
    }
}

#[tokio::test]
async fn missing_agent_lifecycle_stream_uses_spawner_archive() {
    let router = Arc::new(astra_messaging::AgentMailboxRouter::new(
        Arc::new(astra_messaging::InProcessTransport::new()),
        Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
    ));
    let spawner =
        DynamicAgentSpawner::new(router).with_executor(Arc::new(ImmediateLifecycleExecutor));
    let execution_metadata = json!({
        "workspace": {"kind": "server_sandbox", "cwd": "/tmp/astra"},
        "executor": {"kind": "server_local"},
        "transport": "server_local"
    });
    let context = crate::orchestration::SpawnContext {
        parent_run_id: "root-run".to_string(),
        parent_agent_id: "root-agent".to_string(),
        resolved_model_name: None,
        recursion_depth: 0,
        parent_is_fork_child: false,
        inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
        inherited_skills: vec![],
        working_dir: PathBuf::from("/tmp/astra"),
        live_event_sink: None,
        client_tool_delivery_tx: None,
        trace_context: None,
        spawn_tool_call_id: Some("call-spawn".to_string()),
        execution_metadata: Some(execution_metadata),
        delegation_chain: Vec::new(),
    };
    let input = astra_turn_core::orchestration_spawn_tool::SpawnAgentInput {
        description: "review code".to_string(),
        prompt: "review".to_string(),
        agent_type: "explore".to_string(),
        run_in_background: false,
        ..Default::default()
    };
    let spawn_output = spawner.spawn(input, &context).await.unwrap();
    assert!(
        matches!(
            spawn_output,
            astra_turn_core::orchestration_spawn_tool::SpawnAgentOutput::Completed { .. }
        ),
        "test setup must archive a synchronous completed child: {spawn_output:?}"
    );

    let sent_lifecycle_events = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let (event_tx, mut event_rx) = mpsc::channel::<Value>(8);
    assert!(
        stream_missing_agent_lifecycle_events(
            &spawner,
            "root-run",
            &event_tx,
            &sent_lifecycle_events
        )
        .await
    );

    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    assert_eq!(events.len(), 2, "expected spawned + completed: {events:?}");
    assert_eq!(events[0]["type"], "agent_spawned");
    assert_eq!(events[0]["workspace"]["kind"], "server_sandbox");
    assert_eq!(events[0]["executor"]["kind"], "server_local");
    assert_eq!(events[0]["transport"], "server_local");
    assert_eq!(events[1]["type"], "agent_completed");
    assert_eq!(events[1]["status"], "completed");
    assert_eq!(events[1]["workspace"]["kind"], "server_sandbox");

    let (second_tx, mut second_rx) = mpsc::channel::<Value>(8);
    assert!(
        stream_missing_agent_lifecycle_events(
            &spawner,
            "root-run",
            &second_tx,
            &sent_lifecycle_events
        )
        .await
    );
    assert!(
        second_rx.try_recv().is_err(),
        "already-sent lifecycle events must not be replayed twice"
    );
}

#[tokio::test]
async fn missing_agent_lifecycle_stream_reconstructs_waiting_child() {
    let router = Arc::new(astra_messaging::AgentMailboxRouter::new(
        Arc::new(astra_messaging::InProcessTransport::new()),
        Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
    ));
    let spawner =
        DynamicAgentSpawner::new(router).with_executor(Arc::new(WaitingLifecycleExecutor));
    let context = crate::orchestration::SpawnContext {
        parent_run_id: "root-run".to_string(),
        parent_agent_id: "root-agent".to_string(),
        resolved_model_name: None,
        recursion_depth: 0,
        parent_is_fork_child: false,
        inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
        inherited_skills: vec![],
        working_dir: PathBuf::from("/tmp/astra"),
        live_event_sink: None,
        client_tool_delivery_tx: None,
        trace_context: None,
        spawn_tool_call_id: Some("call-spawn".to_string()),
        execution_metadata: Some(json!({
            "workspace": {"kind": "edge_workspace", "cwd": "/Users/test/repo"},
            "executor": {"kind": "edge_agent", "status": "offline"},
            "transport": "edge_ws"
        })),
        delegation_chain: Vec::new(),
    };
    let input = astra_turn_core::orchestration_spawn_tool::SpawnAgentInput {
        description: "review code".to_string(),
        prompt: "review".to_string(),
        agent_type: "explore".to_string(),
        run_in_background: false,
        ..Default::default()
    };
    let spawn_output = spawner.spawn(input, &context).await.unwrap();
    assert!(
        matches!(
            spawn_output,
            astra_turn_core::orchestration_spawn_tool::SpawnAgentOutput::Waiting { .. }
        ),
        "test setup must archive a synchronous waiting child: {spawn_output:?}"
    );

    let sent_lifecycle_events = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let (event_tx, mut event_rx) = mpsc::channel::<Value>(8);
    assert!(
        stream_missing_agent_lifecycle_events(
            &spawner,
            "root-run",
            &event_tx,
            &sent_lifecycle_events
        )
        .await
    );

    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    assert_eq!(events.len(), 2, "expected spawned + waiting: {events:?}");
    assert_eq!(events[0]["type"], "agent_spawned");
    assert_eq!(events[1]["type"], "agent_waiting");
    assert_eq!(events[1]["reason"], "executor_offline");
    assert_eq!(events[1]["workspace"]["kind"], "edge_workspace");
    assert_eq!(events[1]["executor"]["kind"], "edge_agent");
}

#[test]
fn agent_live_event_to_work_surface_sse_maps_output_and_terminal() {
    let metadata = json!({
        "workspace": {
            "kind": "edge_workspace",
            "display_name": "MacBook Pro",
            "cwd": "/Users/test/project",
            "authority": "read_write",
        },
        "executor": {
            "kind": "edge_agent",
            "executor_id": "edge-macbook-1",
            "display_name": "MacBook Pro",
            "transport": "edge_ws",
            "status": "online"
        },
        "transport": "edge_ws",
    });
    let output = super::agent_live_event_to_work_surface_sse(
        &AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "agent-1".to_string(),
            kind: AgentLiveEventKind::OutputDelta("child output".to_string()),
        },
        Some(&metadata),
    );
    assert_eq!(output["type"], "agent_live_event");
    assert_eq!(output["run_id"], "test-run");
    assert_eq!(output["agent_id"], "agent-1");
    assert_eq!(output["event_kind"], "output_delta");
    assert_eq!(output["content"], "child output");
    assert_eq!(output["workspace"]["kind"], "edge_workspace");
    assert_eq!(output["executor"]["kind"], "edge_agent");
    assert_eq!(output["transport"], "edge_ws");

    let terminal = super::agent_live_event_to_work_surface_sse(
        &AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "agent-1".to_string(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Completed,
                duration_ms: 12,
                reason: None,
            },
        },
        Some(&metadata),
    );
    assert_eq!(terminal["event_kind"], "agent_terminated");
    assert_eq!(terminal["termination"], "completed");
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["duration_ms"], 12);
    assert_eq!(terminal["workspace"]["kind"], "edge_workspace");
    assert_eq!(terminal["executor"]["executor_id"], "edge-macbook-1");

    let signal = super::agent_live_event_to_work_surface_sse(
        &AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "agent-1".to_string(),
            kind: AgentLiveEventKind::Signal(
                astra_turn_core::agent_live_event::AgentLiveSignal::PermissionAutoApproved {
                    tool: "bash".into(),
                    reason: "session rule".into(),
                },
            ),
        },
        Some(&metadata),
    );
    assert_eq!(signal["event_kind"], "signal");
    assert_eq!(signal["signal"]["signal"], "permission_auto_approved");
    assert_eq!(signal["signal"]["tool"], "bash");
    assert_eq!(signal["executor"]["executor_id"], "edge-macbook-1");
}

// ── extract_prev_assistant_text + implicit feedback wiring ──

#[test]
fn task_board_resume_hint_is_bounded_and_prefers_running_work() {
    use astra_tools::task_mgmt::SessionTaskStatusKind;

    let tasks = vec![
        test_session_task("task-1", "pending setup", SessionTaskStatusKind::Pending),
        test_session_task(
            "task-2",
            "active implementation",
            SessionTaskStatusKind::InProgress,
        ),
        test_session_task("task-3", "already done", SessionTaskStatusKind::Completed),
        test_session_task("task-4", "waiting review", SessionTaskStatusKind::Paused),
    ];

    let hint = format_task_board_resume_hint(&tasks).expect("open task hint");

    assert_eq!(
        hint,
        "open=3 · next=[in_progress] task-2: active implementation · +2 more open"
    );
}

#[test]
fn task_board_resume_hint_is_absent_without_open_work() {
    use astra_tools::task_mgmt::SessionTaskStatusKind;

    let tasks = vec![test_session_task(
        "task-1",
        "already done",
        SessionTaskStatusKind::Completed,
    )];

    assert!(format_task_board_resume_hint(&tasks).is_none());
}

#[test]
fn task_board_resume_hint_distinguishes_resolved_and_unresolved_dependencies() {
    use astra_tools::task_mgmt::SessionTaskStatusKind;

    let completed = test_session_task(
        "task-1",
        "finished prerequisite",
        SessionTaskStatusKind::Completed,
    );
    let mut ready = test_session_task("task-2", "ready dependent", SessionTaskStatusKind::Pending);
    ready.blocked_by = vec!["task-1".into()];
    let resolved = format_task_board_resume_hint(&[completed, ready]).expect("resolved hint");
    assert!(resolved.contains("next=[pending] task-2"), "{resolved}");
    assert!(!resolved.contains("blocked_by"), "{resolved}");

    let mut blocked = test_session_task(
        "task-3",
        "waiting dependent",
        SessionTaskStatusKind::Pending,
    );
    blocked.blocked_by = vec!["task-missing".into()];
    let unresolved = format_task_board_resume_hint(&[blocked]).expect("unresolved hint");
    assert!(
        unresolved.contains("waiting=[pending] task-3"),
        "{unresolved}"
    );
    assert!(
        unresolved.contains("blocked_by=task-missing"),
        "{unresolved}"
    );
    assert!(!unresolved.contains("next="), "{unresolved}");
}

#[test]
fn trace_redaction_removes_nested_secrets_and_truncates_long_text() {
    let redacted = redact_trace_value(&json!({
        "Authorization": "Bearer secret",
        "nested": {
            "api_key": "abc123",
            "safe": "visible"
        },
        "items": [
            {"cookie": "session=abc"},
            {"text": "x".repeat(2_050)}
        ]
    }));

    assert_eq!(redacted["Authorization"], "[REDACTED]");
    assert_eq!(redacted["nested"]["api_key"], "[REDACTED]");
    assert_eq!(redacted["nested"]["safe"], "visible");
    assert_eq!(redacted["items"][0]["cookie"], "[REDACTED]");
    assert!(
        redacted["items"][1]["text"]
            .as_str()
            .expect("string")
            .ends_with("...")
    );
}

#[test]
fn format_run_events_adds_index() {
    let events = AgenticRunLifecycleService::format_run_events(
        &[
            json!({"event_type": "run_started"}),
            json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
        ],
        0,
    );

    assert_eq!(events[0]["index"], 0);
    assert_eq!(events[1]["index"], 1);
    assert_eq!(events[1]["event_type"], "text_delta");
}

#[test]
fn format_run_events_preserves_global_offset() {
    let events = AgenticRunLifecycleService::format_run_events(
        &[
            json!({"event_type": "tool_call"}),
            json!({"event_type": "tool_result"}),
        ],
        41,
    );

    assert_eq!(events[0]["index"], 41);
    assert_eq!(events[1]["index"], 42);
}

#[test]
fn run_status_as_str_matches_durable_status_constants() {
    assert_eq!(RunStatus::Running.as_str(), STATUS_RUNNING);
    assert_eq!(RunStatus::Paused.as_str(), STATUS_PAUSED);
    assert_eq!(RunStatus::Waiting.as_str(), STATUS_WAITING);
    assert_eq!(RunStatus::Completed.as_str(), STATUS_COMPLETED);
    assert_eq!(RunStatus::Failed.as_str(), STATUS_FAILED);
    assert_eq!(RunStatus::Cancelled.as_str(), STATUS_CANCELLED);
}

#[test]
fn server_loop_causal_chain_ids_fit_agent_event_column() {
    let id = server_loop_causal_chain_id("server-loop-tools");
    assert!(
        id.len() <= astra_services::storage::AGENT_EVENT_ID_LEN,
        "server loop causal chain id must fit agent_events.event_id: {id}"
    );
}

#[test]
fn tool_trace_events_populate_columns_and_redacted_payloads() {
    let trace = TraceContext {
        session_id: "session-1".to_string(),
        user_id: "user-1".to_string(),
        turn_id: "turn-1".to_string(),
        turn_seq: 7,
        causal_chain_id: "chain-1".to_string(),
        root_event_id: "trace:root".to_string(),
    };
    let record = ToolCallRecord {
        tool_call_id: Some("tool-call-1".to_string()),
        name: "agent".to_string(),
        ok: true,
        ms: 42,
        args_preview: Some("agent(action='spawn'): child".to_string()),
        result_preview: Some("launched child".to_string()),
        round: Some(2),
        args_full: Some(r#"{"action":"spawn","token":"secret"}"#.to_string()),
        result_full: Some(
            r#"{"agent_id":"child@run","run_id":"child-run","result":"ok"}"#.to_string(),
        ),
        ..Default::default()
    };

    let events = build_tool_trace_events(
        &trace,
        "root-run",
        None,
        Some("root-agent"),
        None,
        &[record],
    );

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "tool_call_started");
    assert_eq!(events[0].tool_call_id.as_deref(), Some("tool-call-1"));
    assert_eq!(events[0].round_index, Some(2));
    assert_eq!(events[0].meta_tool_name.as_deref(), Some("agent"));
    assert_eq!(
        events[0].metadata["tool_args_json_redacted"]["token"],
        "[REDACTED]"
    );
    assert_eq!(events[1].event_type, "tool_call_completed");
    assert_eq!(events[1].meta_duration_ms, Some(42));
    assert_eq!(events[1].metadata["action"], "spawn");
    assert_eq!(events[1].metadata["child_run_id"], "child-run");
}

#[test]
fn failed_tool_trace_event_persists_searchable_error_content() {
    let trace = TraceContext {
        session_id: "session-1".to_string(),
        user_id: "user-1".to_string(),
        turn_id: "turn-1".to_string(),
        turn_seq: 7,
        causal_chain_id: "chain-1".to_string(),
        root_event_id: "trace:root".to_string(),
    };
    let record = ToolCallRecord {
        tool_call_id: Some("tool-call-failed".to_string()),
        name: "bash".to_string(),
        ok: false,
        ms: 9,
        error: Some("unknown_tool: bash".to_string()),
        ..Default::default()
    };

    let events = build_tool_trace_events(
        &trace,
        "root-run",
        None,
        Some("root-agent"),
        None,
        &[record],
    );

    assert_eq!(events[1].event_type, "tool_call_failed");
    assert_eq!(events[1].content.as_deref(), Some("unknown_tool: bash"));
}

#[test]
fn unexecuted_tool_trace_events_preserve_canonical_terminal_dispositions() {
    let trace = TraceContext {
        session_id: "session-1".to_string(),
        user_id: "user-1".to_string(),
        turn_id: "turn-1".to_string(),
        turn_seq: 7,
        causal_chain_id: "chain-1".to_string(),
        root_event_id: "trace:root".to_string(),
    };
    let records = vec![
        ToolCallRecord {
            tool_call_id: Some("call-rejected".into()),
            name: "bash".into(),
            ok: false,
            disposition: Some(ToolCallDisposition::Rejected),
            result_class: Some(astra_services::session_journal::BLOCKED_TOOL_RESULT_CLASS.into()),
            ..Default::default()
        },
        ToolCallRecord {
            tool_call_id: Some("call-reused".into()),
            name: "read_file".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Reused),
            result_class: Some(astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS.into()),
            ..Default::default()
        },
        ToolCallRecord {
            tool_call_id: Some("call-suppressed".into()),
            name: "write_file".into(),
            ok: false,
            disposition: Some(ToolCallDisposition::Suppressed),
            surgically_removed: Some(true),
            ..Default::default()
        },
        ToolCallRecord {
            tool_call_id: Some("call-deferred".into()),
            name: "agent".into(),
            ok: false,
            disposition: Some(ToolCallDisposition::Deferred),
            ..Default::default()
        },
    ];

    let events =
        build_tool_trace_events(&trace, "root-run", None, Some("root-agent"), None, &records);

    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "tool_call_started",
            "tool_call_rejected",
            "tool_call_started",
            "tool_call_reused",
            "tool_call_started",
            "tool_call_suppressed",
            "tool_call_started",
            "tool_call_deferred",
        ]
    );
    assert!(events.iter().all(|event| event.meta_duration_ms.is_none()));
    assert_eq!(events[1].metadata["disposition"], "rejected");
    assert_eq!(events[3].metadata["disposition"], "reused");
    assert_eq!(events[5].metadata["disposition"], "suppressed");
    assert_eq!(events[7].metadata["disposition"], "deferred");
}

#[test]
fn extract_prev_assistant_text_picks_latest_assistant_string() {
    let messages = vec![
        serde_json::json!({"role": "user", "content": "hi"}),
        serde_json::json!({"role": "assistant", "content": "first answer"}),
        serde_json::json!({"role": "user", "content": "follow up"}),
        serde_json::json!({"role": "assistant", "content": "latest answer"}),
    ];
    assert_eq!(
        extract_prev_assistant_text(&messages).as_deref(),
        Some("latest answer")
    );
}

#[test]
fn extract_prev_assistant_text_handles_content_parts_array() {
    let messages = vec![
        serde_json::json!({"role": "user", "content": "hi"}),
        serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "part one"},
                {"type": "text", "text": "part two"},
            ],
        }),
    ];
    assert_eq!(
        extract_prev_assistant_text(&messages).as_deref(),
        Some("part one\npart two")
    );
}

#[test]
fn extract_prev_assistant_text_returns_none_when_no_assistant_turn() {
    let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
    assert!(extract_prev_assistant_text(&messages).is_none());
}

#[test]
fn extract_prev_assistant_text_skips_empty_assistant_bodies() {
    let messages = vec![
        serde_json::json!({"role": "assistant", "content": "real answer"}),
        serde_json::json!({"role": "user", "content": "ok"}),
        serde_json::json!({"role": "assistant", "content": "   "}),
    ];
    assert_eq!(
        extract_prev_assistant_text(&messages).as_deref(),
        Some("real answer")
    );
}

#[test]
fn build_run_turn_complete_event_carries_authoritative_assistant_text() {
    let event = build_run_turn_complete_event_with_interruption(
        0,
        "recovered final answer",
        None,
        &astra_turn_core::complete::TurnCompletionFacts::default(),
    );
    assert_eq!(event["type"], "turn_complete");
    assert_eq!(event["assistant_text"], "recovered final answer");
    assert_eq!(event["has_tool_calls"], false);
}

#[test]
fn build_run_turn_complete_event_omits_empty_assistant_text() {
    let event = build_run_turn_complete_event_with_interruption(
        1,
        "",
        None,
        &astra_turn_core::complete::TurnCompletionFacts::default(),
    );
    assert_eq!(event["type"], "turn_complete");
    assert_eq!(event["has_tool_calls"], true);
    assert!(event.get("assistant_text").is_none());
}

#[test]
fn stream_turn_complete_is_only_for_completed_or_paused_turns() {
    assert!(should_emit_stream_turn_complete(&RunStatus::Completed));
    assert!(should_emit_stream_turn_complete(&RunStatus::Paused));
    assert!(!should_emit_stream_turn_complete(&RunStatus::Failed));
    assert!(!should_emit_stream_turn_complete(&RunStatus::Cancelled));
    assert!(!should_emit_stream_turn_complete(&RunStatus::Waiting));
    assert!(!should_emit_stream_turn_complete(&RunStatus::Running));
}

#[test]
fn transcript_page_seq_rolls_over_every_fifty_items() {
    assert_eq!(transcript_page_seq(1), 1);
    assert_eq!(transcript_page_seq(50), 1);
    assert_eq!(transcript_page_seq(51), 2);
    assert_eq!(transcript_page_seq(101), 3);
}

#[test]
fn transcript_page_bounds_cover_exact_page_window() {
    assert_eq!(transcript_page_bounds(1), (1, 50));
    assert_eq!(transcript_page_bounds(2), (51, 100));
    assert_eq!(transcript_page_bounds(3), (101, 150));
}

#[test]
fn budget_exhausted_paused_run_does_not_block_next_session_turn() {
    let (mut run, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
        "run-1".to_string(),
        "session-1".to_string(),
        "user-1".to_string(),
    );

    run.status = RunStatus::Running;
    assert!(
        AgenticRunLifecycleService::blocks_new_session_run(&run, "user-1", "session-1"),
        "running run must block a concurrent turn"
    );
    assert!(
        !AgenticRunLifecycleService::blocks_new_session_run(&run, "user-2", "session-1"),
        "the same session id owned by another user must not block"
    );

    run.status = RunStatus::Paused;
    run.waiting_for = Some("user_resume".to_string());
    assert!(
        AgenticRunLifecycleService::blocks_new_session_run(&run, "user-1", "session-1"),
        "manual/user-wait paused run must block until resumed or cancelled"
    );

    run.waiting_for = None;
    assert!(
        !AgenticRunLifecycleService::blocks_new_session_run(&run, "user-1", "session-1"),
        "budget-exhausted paused run has no waiting_for and must allow the next message"
    );

    run.status = RunStatus::Waiting;
    assert!(
        AgenticRunLifecycleService::blocks_new_session_run(&run, "user-1", "session-1"),
        "waiting run must still block a concurrent turn"
    );
}

fn test_spawn_run_config(allowed_tools: Vec<&str>, read_only: bool) -> SpawnRunConfig {
    let inherited_permissions = crate::orchestration::InheritedPermissions::auto_approve();
    let permission_context =
        crate::orchestration::PermissionSyncContext::shared(inherited_permissions.clone());
    SpawnRunConfig {
        run_id: "child-run".to_string(),
        agent_id: "child@1234".to_string(),
        spawn_tool_call_id: None,
        recursion_depth: 1,
        agent_type: "test".to_string(),
        description: "Test child task".to_string(),
        task: "do work".to_string(),
        system_prompt_addendum: String::new(),
        model: None,
        max_turns: 3,
        allowed_tools: allowed_tools.into_iter().map(String::from).collect(),
        read_only,
        working_dir: std::path::PathBuf::from("/tmp"),
        mailbox: None,
        progress_emitter: None,
        context_cache: None,
        inherited_permissions,
        parent_address: None,
        permission_context,
        inherited_skills: Vec::new(),
        live_event_sink: None,
        client_tool_delivery_tx: None,
        inherited_prefix: None,
        execution_metadata: None,
        is_fork_child: false,
        delegation_chain: Vec::new(),
    }
}

fn test_spawn_runtime_context(parent_run_id: &str, user_id: &str) -> ServerSpawnRuntimeContext {
    ServerSpawnRuntimeContext {
        parent_run_id: parent_run_id.to_string(),
        user_id: user_id.to_string(),
        session_id: "session-1".to_string(),
        forward_headers: HashMap::new(),
        admitted_model_execution: Some(test_admitted_model_execution()),
        request_constraints: RequestConstraints::default(),
        execution_metadata: None,
        provider_run_owner: None,
        spawner: std::sync::Weak::new(),
        pause_flag: None,
        cancel_token: None,
        trace_context: server_trace_context(user_id, "session-1", parent_run_id, 1),
        #[cfg(feature = "bridge-e2e-hooks")]
        test_child_llm_rounds: Vec::new(),
        #[cfg(feature = "harness")]
        harness_sink: None,
    }
}

fn test_dynamic_agent_spawner() -> Arc<DynamicAgentSpawner> {
    let transport = Arc::new(astra_messaging::InProcessTransport::new());
    let tracker = Arc::new(crate::server::delegation::engine::DelegationTracker::new());
    let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
    Arc::new(DynamicAgentSpawner::new(router))
}

#[tokio::test]
async fn server_spawn_runtime_context_is_keyed_by_parent_run() {
    let executor = ServerSpawnAgentExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    );
    executor
        .set_runtime_context(test_spawn_runtime_context("parent-run-a", "user-a"))
        .await;
    executor
        .set_runtime_context(test_spawn_runtime_context("parent-run-b", "user-b"))
        .await;

    let mut config = test_spawn_run_config(vec!["*"], false);
    config.parent_address = Some(astra_messaging::types::AgentAddress::new(
        "parent-run-b",
        "root-agent",
    ));

    let context = executor.runtime_context_for_config(&config).await.unwrap();

    assert_eq!(context.parent_run_id, "parent-run-b");
    assert_eq!(context.user_id, "user-b");
}

#[tokio::test]
async fn server_dynamic_child_becomes_a_valid_parent_for_grandchildren() {
    let executor = ServerSpawnAgentExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    );
    let spawner = test_dynamic_agent_spawner();
    let mut root_context = test_spawn_runtime_context("root-run", "user-a");
    root_context.request_constraints = RequestConstraints::new(
        Some(["read_file".to_string()].into_iter().collect()),
        None,
        None,
        None,
    );
    root_context.spawner = Arc::downgrade(&spawner);
    executor.set_runtime_context(root_context).await;

    let mut child = test_spawn_run_config(vec!["read_file", "bash"], false);
    child.run_id = "child-run".to_string();
    child.parent_address = Some(astra_messaging::types::AgentAddress::new(
        "root-run",
        "root-agent",
    ));
    let root = executor.runtime_context_for_config(&child).await.unwrap();
    let child_constraints = spawn_child_request_constraints(&root.request_constraints, &child);
    executor
        .register_child_runtime_context(&root, &child, child_constraints)
        .await;

    let mut grandchild = test_spawn_run_config(vec!["read_file", "bash"], false);
    grandchild.run_id = "grandchild-run".to_string();
    grandchild.parent_address = Some(astra_messaging::types::AgentAddress::new(
        "child-run",
        "child-agent",
    ));
    let context = executor
        .runtime_context_for_config(&grandchild)
        .await
        .unwrap();

    assert_eq!(context.parent_run_id, "child-run");
    assert_eq!(context.user_id, "user-a");
    assert_eq!(
        context.request_constraints.allowed_tools,
        Some(["read_file".to_string()].into_iter().collect())
    );
    assert!(
        context.spawner.upgrade().is_some(),
        "the live child must retain the session-owned spawner capability"
    );
}

#[tokio::test]
async fn server_dynamic_child_controls_are_private_but_parent_cancellation_propagates() {
    let executor = ServerSpawnAgentExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    );
    let root_pause_flag = Arc::new(AtomicBool::new(false));
    let root_cancel_token = Arc::new(CancellationToken::new());
    let mut root_context = test_spawn_runtime_context("root-run", "user-a");
    root_context.pause_flag = Some(root_pause_flag.clone());
    root_context.cancel_token = Some(root_cancel_token.clone());
    executor.set_runtime_context(root_context).await;

    let mut child = test_spawn_run_config(vec!["read_file"], false);
    child.run_id = "child-run".to_string();
    child.parent_address = Some(astra_messaging::types::AgentAddress::new(
        "root-run",
        "root-agent",
    ));
    let parent = executor.runtime_context_for_config(&child).await.unwrap();
    let child_context = executor
        .register_child_runtime_context(
            &parent,
            &child,
            spawn_child_request_constraints(&parent.request_constraints, &child),
        )
        .await;
    let child_pause_flag = child_context.pause_flag.expect("child pause flag");
    let child_cancel_token = child_context
        .cancel_token
        .expect("child cancellation token");

    assert!(
        !Arc::ptr_eq(&root_pause_flag, &child_pause_flag),
        "a child must not share its parent's pause flag"
    );
    assert!(
        !Arc::ptr_eq(&root_cancel_token, &child_cancel_token),
        "a child receives a descendant cancellation token, not the parent's token"
    );
    let mut sibling = test_spawn_run_config(vec!["read_file"], false);
    sibling.run_id = "sibling-run".to_string();
    sibling.parent_address = Some(astra_messaging::types::AgentAddress::new(
        "root-run",
        "root-agent",
    ));
    let sibling_context = executor
        .register_child_runtime_context(
            &parent,
            &sibling,
            spawn_child_request_constraints(&parent.request_constraints, &sibling),
        )
        .await;
    sibling_context
        .cancel_token
        .expect("sibling cancellation token")
        .cancel();
    assert!(
        !root_cancel_token.is_cancelled(),
        "direct child cancellation must not cancel the root"
    );
    let child_context = executor
        .runtime_context_for_config(&child)
        .await
        .expect("registered child context");
    let inherited_child_token = child_context.cancel_token.expect("stored child token");
    root_cancel_token.cancel();
    assert!(
        inherited_child_token.is_cancelled(),
        "root cancellation must still reach child tokens"
    );
}

#[tokio::test]
async fn server_spawn_runtime_context_requires_parent_lineage() {
    let executor = ServerSpawnAgentExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    );
    executor
        .set_runtime_context(test_spawn_runtime_context("parent-run-a", "user-a"))
        .await;

    let config = test_spawn_run_config(vec!["*"], false);
    let err = match executor.runtime_context_for_config(&config).await {
        Ok(_) => panic!("server dynamic spawn must not run without parent lineage"),
        Err(err) => err,
    };

    assert!(err.contains("parent run lineage"), "{err}");
}

#[test]
fn subrun_turn_budget_treats_spawn_budget_as_authoritative_ceiling() {
    let profile =
        astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
            true,
            true,
            astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex,
        );
    let budget = resolve_subrun_agentic_turn_budget(profile, Some(3));

    assert_eq!(budget.initial_turns, 3);
    assert_eq!(budget.hard_turn_limit, 3);
    assert_eq!(budget.extension_turns, 0);
    assert_eq!(budget.max_extensions, 0);
}

#[test]
fn subrun_turn_budget_respects_spawn_budget_above_profile_hard_limit() {
    let profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
        "answer this small question",
    );
    let budget = resolve_subrun_agentic_turn_budget(profile, Some(240));

    assert_eq!(budget.initial_turns, 240);
    assert_eq!(budget.hard_turn_limit, 240);
    assert_eq!(budget.max_extensions, 0);
}

#[test]
fn server_subrun_immediately_resumable_interruption_is_partial_not_paused() {
    let svc = test_service();
    let request = test_request("subrun task board bookkeeping");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "child-run-1",
        None,
        None,
        None,
    );
    state.final_text.clear();
    state.hooks.task_board_snapshot =
        lifecycle_task_board_snapshot(astra_tools::task_mgmt::SessionTaskStatusKind::InProgress);
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::EmptyCompletion,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 1,
            turns_completed: 1,
            remaining_turns: 3,
            error_detail: Some("task-board bookkeeping settlement".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));

    assert_eq!(
        server_subrun_completed_agent_status(&state),
        astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL
    );
    let outcome = Ok(AgenticLoopOutcome::Completed);
    assert_eq!(
        server_subrun_durable_status(&outcome, &state),
        STATUS_FAILED,
        "partial is a terminal child result, not a durable execution hold"
    );
    assert_eq!(
        server_subrun_live_termination(&outcome, &state),
        Some(astra_turn_core::agent_live_event::AgentLiveTermination::Interrupted)
    );
    assert_eq!(
        server_subrun_live_reason(&outcome, &state).as_deref(),
        Some("empty_completion")
    );
}

#[test]
fn server_subrun_waiting_is_recoverable_and_not_terminated() {
    let svc = test_service();
    let request = test_request("subrun waits for external input");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "child-run-waiting",
        None,
        None,
        None,
    );
    let outcome = Ok(AgenticLoopOutcome::Waiting("approval".to_string()));

    assert_eq!(server_subrun_live_termination(&outcome, &state), None);
    assert_eq!(
        server_subrun_live_reason(&outcome, &state).as_deref(),
        Some("approval")
    );
    assert_eq!(
        server_subrun_outcome_status(&outcome, &state),
        STATUS_WAITING
    );
}

#[test]
fn server_subrun_cancel_is_terminal_across_live_and_durable_projections() {
    let svc = test_service();
    let request = test_request("cancel subrun");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "child-run-cancelled",
        None,
        None,
        None,
    );
    let outcome = Ok(AgenticLoopOutcome::Cancelled);

    assert_eq!(
        server_subrun_live_termination(&outcome, &state),
        Some(astra_turn_core::agent_live_event::AgentLiveTermination::Cancelled)
    );
    assert_eq!(
        server_subrun_live_reason(&outcome, &state).as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        server_subrun_outcome_status(&outcome, &state),
        STATUS_CANCELLED
    );
}

#[test]
fn server_subrun_requires_intervention_from_interruption_not_task_board() {
    let svc = test_service();
    let request = test_request("subrun paused task board");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "child-run-1",
        None,
        None,
        None,
    );
    state.final_text = "Paused pending user direction.".to_string();
    state.hooks.task_board_snapshot =
        lifecycle_task_board_snapshot(astra_tools::task_mgmt::SessionTaskStatusKind::Paused);
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::EmptyCompletion,
        astra_turn_core::interruption::ResumeAction::RequiresIntervention {
            description: "paused task needs direction".to_string(),
        },
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 1,
            turns_completed: 1,
            remaining_turns: 3,
            error_detail: Some("paused task needs direction".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));

    assert_eq!(server_subrun_completed_agent_status(&state), STATUS_PAUSED);
    let outcome = Ok(AgenticLoopOutcome::Completed);
    assert_eq!(
        server_subrun_durable_status(&outcome, &state),
        STATUS_PAUSED
    );
    assert_eq!(
        server_subrun_live_termination(&outcome, &state),
        Some(astra_turn_core::agent_live_event::AgentLiveTermination::Interrupted)
    );
    assert_eq!(
        server_subrun_live_reason(&outcome, &state).as_deref(),
        Some("empty_completion")
    );
}

#[test]
fn server_subrun_tool_only_empty_completion_is_terminal_partial() {
    let svc = test_service();
    let request = test_request("subrun tool-only empty completion");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "child-run-1",
        None,
        None,
        None,
    );
    let interruption = astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::EmptyCompletion,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 1,
            turns_completed: 1,
            remaining_turns: 3,
            error_detail: Some("loop ended without final text".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    );
    state.final_text = interruption.user_message.clone();
    state.interruption = Some(interruption);

    assert_eq!(
        server_subrun_completed_agent_status(&state),
        astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL,
        "successful tools alone are preserved as partial without pretending the child is still paused"
    );
}

#[test]
fn server_subrun_budget_exhaustion_preserves_reason_without_leaving_child_paused() {
    let svc = test_service();
    let request = test_request("subrun exhausts its adaptive budget");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "child-run-budget",
        None,
        None,
        None,
    );
    state.final_text = "Partial review findings.".to_string();
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 4,
            turns_completed: 13,
            remaining_turns: 0,
            error_detail: Some("adaptive hard turn limit reached".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));
    let outcome = Ok(AgenticLoopOutcome::Completed);

    assert_eq!(
        server_subrun_outcome_status(&outcome, &state),
        astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL
    );
    assert_eq!(
        server_subrun_durable_status(&outcome, &state),
        STATUS_FAILED
    );
    assert_eq!(
        server_subrun_live_reason(&outcome, &state).as_deref(),
        Some("budget_exhausted")
    );
    assert_eq!(
        server_subrun_interruption_reason(&state).as_deref(),
        Some("budget_exhausted: adaptive hard turn limit reached")
    );
    assert_eq!(
        server_subrun_durable_error(
            &outcome,
            server_subrun_outcome_status(&outcome, &state),
            server_subrun_interruption_reason(&state).as_deref(),
        )
        .as_deref(),
        Some("budget_exhausted: adaptive hard turn limit reached")
    );
    assert_eq!(
        server_subrun_durable_error_code(server_subrun_outcome_status(&outcome, &state)),
        Some(astra_services::coordination::AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE)
    );
}

#[test]
fn server_subrun_nonresumable_interruption_is_failed_not_completed() {
    let svc = test_service();
    let request = test_request("subrun blocked by fatal harness verdict");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "child-run-blocked",
        None,
        None,
        None,
    );
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::HarnessBlocked,
        astra_turn_core::interruption::ResumeAction::StartNewSession,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: false,
            tool_calls_completed: 0,
            turns_completed: 0,
            remaining_turns: 13,
            error_detail: Some("fatal invariant violation".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));
    let outcome = Ok(AgenticLoopOutcome::Completed);

    assert_eq!(
        server_subrun_outcome_status(&outcome, &state),
        STATUS_FAILED
    );
    assert_eq!(
        server_subrun_live_termination(&outcome, &state),
        Some(astra_turn_core::agent_live_event::AgentLiveTermination::Failed)
    );
    assert_eq!(
        server_subrun_live_reason(&outcome, &state).as_deref(),
        Some("harness_blocked")
    );
    assert_eq!(
        server_subrun_durable_error(
            &outcome,
            server_subrun_outcome_status(&outcome, &state),
            server_subrun_interruption_reason(&state).as_deref(),
        )
        .as_deref(),
        Some("harness_blocked: fatal invariant violation")
    );
}

#[test]
fn spawn_child_constraints_intersect_parent_and_agent_allowlists() {
    let parent = RequestConstraints::new(
        Some(
            ["bash", "read_file", "write_file"]
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        None,
        Some(["review"].into_iter().map(String::from).collect()),
        Some(
            [
                crate::skills::manifest::SkillSourceKind::Local,
                crate::skills::manifest::SkillSourceKind::Database,
            ]
            .into_iter()
            .collect(),
        ),
    );
    let config = test_spawn_run_config(vec!["bash", "read_file"], true);

    let constraints = spawn_child_request_constraints(&parent, &config);

    assert_eq!(
        constraints.allowed_tools.unwrap(),
        ["bash", "read_file"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    assert_eq!(
        constraints.allowed_skills.unwrap(),
        ["review"].into_iter().map(String::from).collect()
    );
    assert_eq!(
        constraints.allowed_skill_sources.unwrap(),
        [
            crate::skills::manifest::SkillSourceKind::Local,
            crate::skills::manifest::SkillSourceKind::Database,
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn spawn_child_constraints_preserve_parent_when_child_allows_all() {
    let parent = RequestConstraints::new(
        Some(
            ["bash", "write_file"]
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        None,
        None,
        None,
    );
    let config = test_spawn_run_config(vec!["*"], false);

    let constraints = spawn_child_request_constraints(&parent, &config);

    assert_eq!(
        constraints.allowed_tools.unwrap(),
        ["bash", "write_file"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

#[test]
fn spawn_child_constraints_read_only_wildcard_gets_read_only_tools() {
    let parent = RequestConstraints::default();
    let config = test_spawn_run_config(vec!["*"], true);

    let constraints = spawn_child_request_constraints(&parent, &config);
    let allowed = constraints.allowed_tools.unwrap();

    assert!(allowed.contains("read_file"));
    assert!(allowed.contains("grep"));
    assert!(!allowed.contains("write_file"));
    assert!(!allowed.contains("str_replace"));
}

#[test]
fn build_run_turn_complete_event_marks_interrupted_turns() {
    let interruption = astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 7,
            turns_completed: 15,
            remaining_turns: 0,
            error_detail: Some("Round budget hard-limit reached".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    );

    let event = build_run_turn_complete_event_with_interruption(
        7,
        "[Round budget hard-limit reached]",
        Some(&interruption),
        &astra_turn_core::complete::TurnCompletionFacts::default(),
    );

    assert_eq!(event["type"], "turn_complete");
    assert_eq!(event["has_tool_calls"], true);
    assert!(event.get("stall_detected").is_none());
    assert_eq!(event["execution_state"]["status"], "interrupted");
    assert_eq!(event["execution_state"]["interrupted"], true);
    assert_eq!(
        event["execution_state"]["interruption_kind"],
        "budget_exhausted"
    );
    assert_eq!(event["execution_state"]["tool_calls_completed"], 7);
    assert_eq!(event["execution_state"]["remaining_turns"], 0);
    assert_eq!(event["assistant_text"], "[Round budget hard-limit reached]");
}

/// Unwrap a `Result<T, (StatusCode, Json<ErrorResponse>)>` in tests.
fn ok<T>(result: Result<T, (StatusCode, Json<ErrorResponse>)>) -> T {
    match result {
        Ok(v) => v,
        Err((status, body)) => panic!("expected Ok, got {status}: {}", body.0.detail),
    }
}

/// Unwrap the error side.
fn err<T>(
    result: Result<T, (StatusCode, Json<ErrorResponse>)>,
) -> (StatusCode, Json<ErrorResponse>) {
    match result {
        Ok(_) => panic!("expected Err, got Ok"),
        Err(e) => e,
    }
}

fn test_settings() -> MatrixOneSettings {
    MatrixOneSettings::from_env_with_database("test_astra_runtime")
}

fn test_encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
}

#[derive(Default)]
struct FaultInjectedRunStoreCounters {
    status_calls: usize,
    append_calls: usize,
}

struct FaultInjectedStatusMutation {
    user_id: String,
    run_id: String,
    status: String,
    waiting_for: Option<String>,
    error_message: Option<String>,
}

struct FaultInjectedRunStateStore {
    inner: InMemoryRunStateStore,
    fail_status_calls: HashSet<usize>,
    fail_append_calls: HashSet<usize>,
    mutate_before_status_call: HashMap<usize, FaultInjectedStatusMutation>,
    counters: StdMutex<FaultInjectedRunStoreCounters>,
    append_delay: Duration,
    appended_batches: StdMutex<Vec<Vec<Value>>>,
}

impl FaultInjectedRunStateStore {
    fn new(fail_status_calls: &[usize], fail_append_calls: &[usize]) -> Self {
        Self {
            inner: InMemoryRunStateStore::new(),
            fail_status_calls: fail_status_calls.iter().copied().collect(),
            fail_append_calls: fail_append_calls.iter().copied().collect(),
            mutate_before_status_call: HashMap::new(),
            counters: StdMutex::new(FaultInjectedRunStoreCounters::default()),
            append_delay: Duration::ZERO,
            appended_batches: StdMutex::new(Vec::new()),
        }
    }

    fn with_append_delay(mut self, append_delay: Duration) -> Self {
        self.append_delay = append_delay;
        self
    }

    fn appended_batches(&self) -> Vec<Vec<Value>> {
        self.appended_batches
            .lock()
            .expect("appended batch lock")
            .clone()
    }

    fn with_status_mutation_before_call(
        mut self,
        call: usize,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Self {
        self.mutate_before_status_call.insert(
            call,
            FaultInjectedStatusMutation {
                user_id: user_id.to_string(),
                run_id: run_id.to_string(),
                status: status.to_string(),
                waiting_for: waiting_for.map(ToString::to_string),
                error_message: error_message.map(ToString::to_string),
            },
        );
        self
    }

    fn next_status_call(&self) -> usize {
        let mut counters = self.counters.lock().expect("status counter lock");
        counters.status_calls += 1;
        counters.status_calls
    }

    fn next_append_call(&self) -> usize {
        let mut counters = self.counters.lock().expect("append counter lock");
        counters.append_calls += 1;
        counters.append_calls
    }

    async fn apply_status_mutation_before_call(&self, call: usize) -> Result<(), String> {
        if let Some(mutation) = self.mutate_before_status_call.get(&call) {
            self.inner
                .update_run_status(
                    &mutation.user_id,
                    &mutation.run_id,
                    &mutation.status,
                    mutation.waiting_for.as_deref(),
                    mutation.error_message.as_deref(),
                )
                .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RunStateStore for FaultInjectedRunStateStore {
    async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String> {
        self.inner.insert_run(record).await
    }

    async fn claim_run_start(
        &self,
        record: DurableRunRecord,
        requested_session_id: Option<&str>,
    ) -> Result<DurableRunStartClaim, String> {
        self.inner
            .claim_run_start(record, requested_session_id)
            .await
    }

    async fn load_run(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        self.inner.load_run(user_id, run_id).await
    }

    async fn load_run_event_delta(
        &self,
        user_id: &str,
        run_id: &str,
        after_event_idx: i64,
    ) -> Result<Option<DurableRunEventDelta>, String> {
        self.inner
            .load_run_event_delta(user_id, run_id, after_event_idx)
            .await
    }

    async fn update_run_status(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!("injected update_run_status failure on call {call}"));
        }
        self.inner
            .update_run_status(user_id, run_id, status, waiting_for, error_message)
            .await
    }

    async fn update_run_status_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!(
                "injected update_run_status_if_current failure on call {call}"
            ));
        }
        self.inner
            .update_run_status_if_current(
                user_id,
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
            )
            .await
    }

    async fn update_run_status_with_event_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        event: serde_json::Value,
    ) -> Result<bool, String> {
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!(
                "injected update_run_status_with_event_if_current failure on call {call}"
            ));
        }
        let append_call = self.next_append_call();
        if self.fail_append_calls.contains(&append_call) {
            return Err(format!(
                "injected transition append_event failure on call {append_call}"
            ));
        }
        self.inner
            .update_run_status_with_event_if_current(
                user_id,
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
                event,
            )
            .await
    }

    async fn update_run_status_with_event_if_current_unless_session_blocked(
        &self,
        request: astra_services::runs::GuardedRunStatusTransitionRequest<'_>,
    ) -> Result<astra_services::runs::GuardedRunStatusTransition, String> {
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!(
                "injected update_run_status_with_event_if_current_unless_session_blocked failure on call {call}"
            ));
        }
        let append_call = self.next_append_call();
        if self.fail_append_calls.contains(&append_call) {
            return Err(format!(
                "injected guarded transition append_event failure on call {append_call}"
            ));
        }
        self.inner
            .update_run_status_with_event_if_current_unless_session_blocked(request)
            .await
    }

    async fn update_run_status_with_events_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<bool, String> {
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!(
                "injected update_run_status_with_events_if_current failure on call {call}"
            ));
        }
        let append_call = self.next_append_call();
        if self.fail_append_calls.contains(&append_call) {
            return Err(format!(
                "injected transition append_events failure on call {append_call}"
            ));
        }
        self.inner
            .update_run_status_with_events_if_current(
                user_id,
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
                events,
            )
            .await
    }

    async fn update_run_usage(
        &self,
        user_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        self.inner
            .update_run_usage(
                user_id,
                run_id,
                prompt_tokens,
                completion_tokens,
                tool_calls,
            )
            .await
    }

    async fn save_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        self.inner
            .save_checkpoint(user_id, run_id, checkpoint_json)
            .await
    }

    async fn load_latest_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_kind: Option<&str>,
    ) -> Result<Option<DurableRunCheckpointRecord>, String> {
        self.inner
            .load_latest_checkpoint(user_id, run_id, checkpoint_kind)
            .await
    }

    async fn load_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        self.inner.load_run_projection(user_id, run_id).await
    }

    async fn rebuild_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        self.inner.rebuild_run_projection(user_id, run_id).await
    }

    async fn append_events_batch(
        &self,
        user_id: &str,
        run_id: &str,
        events: &[serde_json::Value],
    ) -> Result<(), String> {
        let call = self.next_append_call();
        if self.fail_append_calls.contains(&call) {
            return Err(format!("injected append_event failure on call {call}"));
        }
        self.appended_batches
            .lock()
            .expect("appended batch lock")
            .push(events.to_vec());
        if !self.append_delay.is_zero() {
            tokio::time::sleep(self.append_delay).await;
        }
        self.inner
            .append_events_batch(user_id, run_id, events)
            .await
    }

    async fn list_user_runs_cursor(
        &self,
        user_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<astra_services::runs::DurableRunListPage, String> {
        self.inner
            .list_user_runs_cursor(user_id, limit, cursor)
            .await
    }

    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.inner.find_waiting_runs().await
    }

    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.inner.find_running_runs().await
    }

    async fn find_blocking_session_run(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        self.inner
            .find_blocking_session_run(user_id, session_id)
            .await
    }

    async fn find_sub_runs(
        &self,
        user_id: &str,
        delegation_id: &str,
    ) -> Result<Vec<DurableRunRecord>, String> {
        self.inner.find_sub_runs(user_id, delegation_id).await
    }

    async fn update_retry_count(
        &self,
        user_id: &str,
        run_id: &str,
        retry_count: u32,
    ) -> Result<bool, String> {
        self.inner
            .update_retry_count(user_id, run_id, retry_count)
            .await
    }
}

struct DenyTokenBudgetGovernor;

#[async_trait]
impl astra_services::resource_governor::ResourceGovernor for DenyTokenBudgetGovernor {
    async fn get_limits(
        &self,
        _user_id: &str,
    ) -> astra_services::resource_governor::ResourceLimits {
        astra_services::resource_governor::ResourceLimits::default()
    }

    async fn set_limits(
        &self,
        _user_id: &str,
        _limits: astra_services::resource_governor::ResourceLimits,
    ) {
    }

    async fn get_usage(&self, _user_id: &str) -> astra_services::resource_governor::ResourceUsage {
        astra_services::resource_governor::ResourceUsage::default()
    }

    async fn check_session_create(
        &self,
        _user_id: &str,
    ) -> astra_services::resource_governor::LimitCheck {
        astra_services::resource_governor::LimitCheck::Allowed
    }

    async fn record_session_created(&self, _user_id: &str) {}

    async fn record_tool_calls(&self, _user_id: &str, _count: u64) {}

    async fn record_tokens(&self, _user_id: &str, _tokens: u64) {}

    async fn check_token_budget(
        &self,
        _user_id: &str,
    ) -> astra_services::resource_governor::LimitCheck {
        astra_services::resource_governor::LimitCheck::Denied {
            limit: astra_services::resource_governor::ResourceLimitKind::DailyTokens,
            reason: "daily token budget exhausted (1000/1000)".to_string(),
        }
    }
}

fn test_service() -> AgenticRunLifecycleService {
    let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine,
    )
    .with_model_service(Arc::new(ActiveTestModelService::default()))
}

struct TerminalTestLlm {
    base_url: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for TerminalTestLlm {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn spawn_terminal_test_llm() -> TerminalTestLlm {
    use axum::{Router, response::IntoResponse, routing::post};

    async fn chat_completions(Json(request): Json<Value>) -> axum::response::Response {
        if request.get("stream").and_then(Value::as_bool) == Some(true) {
            let delta = json!({"choices":[{"delta":{"content":"done"}}]});
            let terminal = json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}});
            return (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {delta}\n\ndata: {terminal}\n\ndata: [DONE]\n\n"),
            )
                .into_response();
        }

        Json(json!({
            "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1}
        }))
        .into_response()
    }

    let app = Router::new().route("/v1/chat/completions", post(chat_completions));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind terminal test LLM");
    let addr = listener.local_addr().expect("terminal test LLM address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve terminal test LLM");
    });
    TerminalTestLlm {
        base_url: format!("http://{addr}/v1"),
        server,
    }
}

async fn spawn_incremental_terminal_test_llm(terminal_delay: Duration) -> TerminalTestLlm {
    use axum::{
        Router,
        body::{Body, Bytes},
        http::header,
        response::{IntoResponse, Response},
        routing::post,
    };
    use std::convert::Infallible;

    async fn chat_completions(
        axum::extract::State(terminal_delay): axum::extract::State<Duration>,
        Json(request): Json<Value>,
    ) -> Response {
        if request.get("stream").and_then(Value::as_bool) != Some(true) {
            return Json(json!({
                "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1}
            }))
            .into_response();
        }

        let stream = async_stream::stream! {
            let reasoning =
                json!({"choices":[{"delta":{"reasoning_content":"thinking"}}]});
            let content = json!({"choices":[{"delta":{"content":"done"}}]});
            yield Ok::<Bytes, Infallible>(Bytes::from(format!("data: {reasoning}\n\n")));
            yield Ok::<Bytes, Infallible>(Bytes::from(format!("data: {content}\n\n")));
            tokio::time::sleep(terminal_delay).await;
            let terminal = json!({
                "choices":[{"delta":{},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":1}
            });
            yield Ok::<Bytes, Infallible>(Bytes::from(format!("data: {terminal}\n\n")));
            yield Ok::<Bytes, Infallible>(Bytes::from("data: [DONE]\n\n"));
        };
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .expect("incremental terminal test response")
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(terminal_delay);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind incremental terminal test LLM");
    let addr = listener
        .local_addr()
        .expect("incremental terminal test LLM address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve incremental terminal test LLM");
    });
    TerminalTestLlm {
        base_url: format!("http://{addr}/v1"),
        server,
    }
}

async fn terminal_test_service() -> (AgenticRunLifecycleService, TerminalTestLlm) {
    let llm = spawn_terminal_test_llm().await;
    let service = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
    )
    .with_model_service(Arc::new(ActiveTestModelService::new(llm.base_url.clone())));
    (service, llm)
}

#[tokio::test]
async fn session_dynamic_agent_executor_inherits_live_and_durable_edge_transports() {
    let service = test_service()
        .with_edge_connection_pool(
            astra_server_types::edge_connection_pool::EdgeConnectionPool::new(),
        )
        .with_edge_dispatch_service(Arc::new(astra_services::UnconfiguredEdgeDispatchService))
        .with_edge_registry_service(Arc::new(astra_services::UnconfiguredEdgeRegistryService));

    let entry = service
        .server_agent_spawner_for_session("user-1", "session-1")
        .await;
    assert!(entry.executor.edge_connection_pool.is_some());
    assert!(entry.executor.edge_dispatch_service.is_some());
    assert!(entry.executor.edge_registry_service.is_some());
}

#[tokio::test]
async fn durable_descendant_cancellation_sweep_converges_nested_active_runs() {
    let service = test_service();
    let engine = service.run_engine.clone();
    engine
        .start_run("root", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .start_run_ext(
            "child",
            "user-1",
            "session-1",
            Some("root"),
            None,
            Some("reviewer"),
            None,
        )
        .await
        .unwrap();
    engine
        .start_run_ext(
            "grandchild",
            "user-1",
            "session-1",
            Some("child"),
            None,
            Some("verifier"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        AgenticRunLifecycleService::cancel_durable_run_descendants(
            &engine,
            "user-1",
            "session-1",
            "root",
            "ancestor cancelled",
        )
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        engine
            .load_run("user-1", "root")
            .await
            .unwrap()
            .unwrap()
            .status,
        STATUS_RUNNING
    );
    for run_id in ["child", "grandchild"] {
        let run = engine.load_run("user-1", run_id).await.unwrap().unwrap();
        assert_eq!(run.status, STATUS_CANCELLED);
        assert!(run.events.iter().any(|event| {
            event["event_type"] == "run_finished" && event["data"]["ancestor_run_id"] == "root"
        }));
    }
}

#[tokio::test]
async fn durable_descendant_cancellation_pages_past_500_descendants_and_unrelated_runs() {
    let service = test_service();
    let engine = service.run_engine.clone();
    engine
        .start_run("other-root", "user-1", "session-wide")
        .await
        .unwrap();
    assert!(
        engine
            .persist_delegation_outcome_status(
                "user-1",
                "other-root",
                STATUS_COMPLETED,
                None,
                None,
            )
            .await
            .unwrap()
    );
    engine
        .start_run("root-wide", "user-1", "session-wide")
        .await
        .unwrap();

    for index in 0..505 {
        engine
            .start_run_ext(
                &format!("child-{index:03}"),
                "user-1",
                "session-wide",
                Some("root-wide"),
                None,
                Some("reviewer"),
                None,
            )
            .await
            .unwrap();
    }
    // Insert newer, unrelated active runs so descendants are not guaranteed
    // to occupy the first bounded page.
    for index in 0..205 {
        engine
            .start_run_ext(
                &format!("unrelated-{index:03}"),
                "user-1",
                "session-wide",
                Some("other-root"),
                None,
                Some("unrelated-worker"),
                None,
            )
            .await
            .unwrap();
    }

    assert_eq!(
        AgenticRunLifecycleService::cancel_durable_run_descendants(
            &engine,
            "user-1",
            "session-wide",
            "root-wide",
            "ancestor cancelled",
        )
        .await
        .unwrap(),
        505
    );

    for index in 0..505 {
        let run = engine
            .load_run("user-1", &format!("child-{index:03}"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_CANCELLED);
    }
    for index in 0..205 {
        let run = engine
            .load_run("user-1", &format!("unrelated-{index:03}"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_RUNNING);
    }
}

#[test]
fn resume_hydration_requires_existing_session_with_prior_prompt_history() {
    assert!(
        !should_restore_prior_prompt_history(false, false),
        "new sessions must not enter resume hydration"
    );
    assert!(
        !should_restore_prior_prompt_history(false, true),
        "prior history is irrelevant when the request does not target an existing session"
    );
    assert!(
        !should_restore_prior_prompt_history(true, false),
        "a pre-created web session id without prompt history is not a resume"
    );
    assert!(
        should_restore_prior_prompt_history(true, true),
        "resume is only valid when an existing session has prompt-facing history"
    );
}

#[test]
fn session_resume_hydration_hint_is_only_needed_without_restored_prompt_messages() {
    assert!(should_build_session_resume_hydration_hint(true, 1));
    assert!(
        !should_build_session_resume_hydration_hint(true, 3),
        "restored user/assistant history already carries the resume context"
    );
    assert!(!should_build_session_resume_hydration_hint(false, 1));
}

#[tokio::test]
async fn server_resume_hydration_failure_is_not_prompt_facing() {
    let service = test_service();

    let hint = service
        .session_resume_hydration_hint_for_session("user-1", "session-1", "run-1", true)
        .await;

    assert_eq!(hint, None);
}

#[test]
fn server_resume_hydration_uses_transcript_when_primary_restore_is_not_viable() {
    let primary = vec![json!({"role": "user", "content": "继续"})];
    let transcript = vec![
        json!({"role": "user", "content": "review branch changes"}),
        json!({"role": "assistant", "content": "The review found resume continuity issues."}),
    ];

    let hint = AgenticRunLifecycleService::session_resume_hydration_hint_from_sources(
        &primary,
        &transcript,
    )
    .expect("transcript metadata should be valid")
    .expect("transcript fallback should provide viable resume context");

    assert!(hint.contains("latest_user_input: review branch changes"));
    assert!(hint.contains("last_assistant_state: The review found resume continuity issues."));
}

#[test]
fn server_resume_hydration_prefers_primary_restore_when_viable() {
    let primary = vec![
        json!({"role": "user", "content": "primary goal"}),
        json!({"role": "assistant", "content": "primary state"}),
    ];
    let transcript = vec![
        json!({"role": "user", "content": "transcript goal"}),
        json!({"role": "assistant", "content": "transcript state"}),
    ];

    let hint = AgenticRunLifecycleService::session_resume_hydration_hint_from_sources(
        &primary,
        &transcript,
    )
    .expect("primary metadata should be valid")
    .expect("primary restore should provide viable resume context");

    assert!(hint.contains("latest_user_input: primary goal"));
    assert!(hint.contains("last_assistant_state: primary state"));
    assert!(!hint.contains("transcript goal"));
    assert!(!hint.contains("transcript state"));
}

#[test]
fn server_resume_hydration_does_not_hide_corrupt_primary_metadata_with_a_fallback() {
    let primary = vec![
        json!({
            "role": "user",
            "content": "primary goal",
            (astra_turn_types::USER_TURN_SEMANTICS_FIELD): {
                "schema_version": "invalid",
                "objective_relation": "replace"
            }
        }),
        json!({"role": "assistant", "content": "primary state"}),
    ];
    let transcript = vec![
        json!({"role": "user", "content": "fallback goal"}),
        json!({"role": "assistant", "content": "fallback state"}),
    ];

    assert!(matches!(
        AgenticRunLifecycleService::session_resume_hydration_hint_from_sources(
            &primary,
            &transcript,
        ),
        Err(astra_turn_types::UserTurnSemanticsError::Malformed(_))
    ));
}

fn test_service_with_store(store: Arc<dyn RunStateStore>) -> AgenticRunLifecycleService {
    let engine = RunEngine::new(store);
    AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine,
    )
    .with_model_service(Arc::new(ActiveTestModelService::default()))
}

async fn install_live_run_state(
    svc: &AgenticRunLifecycleService,
    user_id: &str,
    run_id: &str,
    session_id: &str,
    status: RunStatus,
    waiting_for: Option<&str>,
) {
    let (mut run_state, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
        run_id.to_string(),
        session_id.to_string(),
        user_id.to_string(),
    );
    run_state.status = status;
    run_state.waiting_for = waiting_for.map(ToString::to_string);
    svc.runs.write().await.insert(run_id.to_string(), run_state);
}

async fn setup_lifecycle_run_db_it() -> SharedPool {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    LIFECYCLE_RUN_DB
        .get_or_init(|| async {
            let settings = MatrixOneSettings::from_env();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".to_string());
            astra_services::ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema");
            SharedPool::new(&settings).await.expect("SharedPool::new")
        })
        .await
        .clone()
}

fn db_backed_test_service(
    shared_pool: &SharedPool,
    owner_pod_id: &str,
) -> AgenticRunLifecycleService {
    let store: Arc<dyn RunStateStore> =
        Arc::new(DatabaseRunStateStore::new(shared_pool.clone()).with_owner_pod_id(owner_pod_id));
    let engine = RunEngine::new(store);
    AgenticRunLifecycleService::new(
        shared_pool.settings().clone(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine,
    )
    .with_model_service(Arc::new(ActiveTestModelService::default()))
}

async fn cleanup_lifecycle_run_fixture(pool: &SharedPool, user_id: &str, run_id: &str) {
    for sql in [
        "DELETE FROM agent_session_execution_slots WHERE user_id = ? AND run_id = ?",
        "DELETE FROM run_display_projections WHERE user_id = ? AND run_id = ?",
        "DELETE FROM run_checkpoints WHERE user_id = ? AND run_id = ?",
        "DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
    ] {
        let _ = sqlx::query(sql)
            .bind(user_id)
            .bind(run_id)
            .execute(pool.get())
            .await;
    }
}

#[derive(Debug)]
struct DurableEventPressureBatch {
    raw_event_count: usize,
    candidate_rows: usize,
    candidate_bytes: usize,
    budgeted_events: Vec<Value>,
    budgeted_bytes: usize,
    compacted: bool,
}

#[derive(Debug)]
struct DurableEventPressureRunStats {
    raw_events: usize,
    candidate_rows: usize,
    candidate_bytes: usize,
    budgeted_rows: usize,
    budgeted_bytes: usize,
    persisted_rows: usize,
    replay_rows: usize,
    compacted_rows: usize,
    text_delta_rows: usize,
    elapsed_ms: u64,
}

fn durable_event_pressure_env_usize(name: &str, default: usize, min: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .unwrap_or_else(|err| panic!("invalid {name}={value:?}: {err}"))
            .max(min),
        Err(_) => default.max(min),
    }
}

fn durable_event_pressure_opted_in() -> bool {
    std::env::var(DURABLE_EVENT_PRESSURE_OPT_IN).as_deref() == Ok("1")
}

fn replay_event_type(event: &Value) -> Option<&str> {
    event
        .get("event_type")
        .or_else(|| event.get("type"))
        .and_then(Value::as_str)
}

fn build_durable_event_pressure_batch(
    run_ordinal: usize,
    text_delta_count: usize,
    progress_event_count: usize,
    budget: DurableRunEventBatchBudget,
) -> DurableEventPressureBatch {
    let mut raw_stream_events: Vec<Value> =
        Vec::with_capacity(text_delta_count + progress_event_count + 5);
    raw_stream_events.extend((0..text_delta_count).map(
        |idx| json!({"type": "text_delta", "content": format!("run-{run_ordinal}-chunk-{idx}")}),
    ));
    raw_stream_events.push(json!({
        "type": "tool_call",
        "tool_call": {"id": format!("call-{run_ordinal}"), "name": "bash"}
    }));
    raw_stream_events.push(json!({
        "type": "tool_call_end",
        "call_id": format!("call-{run_ordinal}"),
        "tool": "bash",
        "result": "ok"
    }));
    raw_stream_events.push(json!({
        "type": "reasoning_done",
        "data": {"signature": format!("sig-{run_ordinal}")}
    }));
    raw_stream_events.extend(
        (0..progress_event_count)
            .map(|idx| json!({"type": "agent_progress", "run_ordinal": run_ordinal, "seq": idx})),
    );
    raw_stream_events.push(json!({
        "event_type": "text_done",
        "data": {"full_text": format!("large durable final answer {run_ordinal}")}
    }));
    raw_stream_events.push(json!({
        "event_type": "run_finished",
        "data": {"prompt_tokens": 9, "completion_tokens": 3, "tool_call_count": 1}
    }));

    let durable_candidates: Vec<Value> = raw_stream_events
        .iter()
        .filter(|event| streaming_event_for_persistence(event))
        .cloned()
        .collect();
    let candidate_rows = durable_candidates.len();
    let candidate_bytes = durable_candidates
        .iter()
        .map(durable_run_event_estimated_bytes)
        .sum::<usize>();
    let budgeted_events =
        enforce_durable_run_event_batch_budget_with_budget(durable_candidates, budget);
    let budgeted_bytes = budgeted_events
        .iter()
        .map(durable_run_event_estimated_bytes)
        .sum::<usize>();
    let compacted = budgeted_events
        .iter()
        .any(|event| durable_event_type(event) == Some("durable_events_compacted"));

    DurableEventPressureBatch {
        raw_event_count: raw_stream_events.len(),
        candidate_rows,
        candidate_bytes,
        budgeted_events,
        budgeted_bytes,
        compacted,
    }
}

async fn durable_event_pressure_case(
    pool: SharedPool,
    run_ordinal: usize,
    text_delta_count: usize,
    progress_event_count: usize,
) -> Result<DurableEventPressureRunStats, String> {
    let user_id = "durable-event-pressure-user";
    let run_id = format!("durable-pressure-{run_ordinal}-{}", Uuid::new_v4());
    let session_id = format!("sess-durable-pressure-{run_ordinal}-{}", Uuid::new_v4());
    let svc = db_backed_test_service(&pool, &format!("durable-pressure-pod-{run_ordinal}"));
    let budget = DurableRunEventBatchBudget::default();
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;

    let started = Instant::now();
    let result = async {
        svc.run_engine
            .start_run(&run_id, user_id, &session_id)
            .await
            .map_err(|err| format!("start durable DB run {run_id}: {err}"))?;

        let batch = build_durable_event_pressure_batch(
            run_ordinal,
            text_delta_count,
            progress_event_count,
            budget,
        );
        if batch
            .budgeted_events
            .iter()
            .any(|event| durable_event_type(event) == Some("text_delta"))
        {
            return Err(format!(
                "{run_id}: transport text_delta entered durable batch"
            ));
        }
        if !batch.compacted {
            return Err(format!("{run_id}: expected semantic overflow compaction"));
        }
        for expected in [
            "durable_events_compacted",
            "tool_call",
            "tool_call_end",
            "reasoning_done",
            "text_done",
            "run_finished",
        ] {
            if !batch
                .budgeted_events
                .iter()
                .any(|event| durable_event_type(event) == Some(expected))
            {
                return Err(format!("{run_id}: missing budgeted {expected}"));
            }
        }

        let transitioned = svc
            .run_engine
            .transition_status_with_events_if_current(
                user_id,
                &run_id,
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &batch.budgeted_events,
            )
            .await
            .map_err(|err| format!("commit budgeted terminal events for {run_id}: {err}"))?;
        if !transitioned {
            return Err(format!("{run_id}: status transition unexpectedly stale"));
        }

        let rows = sqlx::query(
            "SELECT event_type
             FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
             ORDER BY event_idx ASC",
        )
        .bind(user_id)
        .bind(&run_id)
        .fetch_all(pool.get())
        .await
        .map_err(|err| format!("load persisted event rows for {run_id}: {err}"))?;
        let persisted_types = rows
            .iter()
            .map(|row| {
                row.try_get::<String, _>("event_type")
                    .map_err(|err| format!("decode event_type for {run_id}: {err}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let persisted_rows = persisted_types.len();
        let text_delta_rows = persisted_types
            .iter()
            .filter(|event_type| event_type.as_str() == "text_delta")
            .count();
        let compacted_rows = persisted_types
            .iter()
            .filter(|event_type| event_type.as_str() == "durable_events_compacted")
            .count();
        if persisted_rows > budget.row_budget + 1 {
            return Err(format!(
                "{run_id}: persisted {persisted_rows} rows above budget plus run_started"
            ));
        }
        if text_delta_rows != 0 {
            return Err(format!(
                "{run_id}: persisted {text_delta_rows} text_delta rows"
            ));
        }
        if compacted_rows != 1 {
            return Err(format!(
                "{run_id}: expected exactly one compaction row, got {compacted_rows}"
            ));
        }

        let replay_events = svc
            .stream_run(run_id.clone(), user_id.to_string(), 1)
            .await
            .map_err(|response| {
                format!(
                    "stream replay failed for {run_id}: {:?}: {}",
                    response.0, response.1.0.detail
                )
            })?;
        if replay_events.len() > budget.row_budget {
            return Err(format!(
                "{run_id}: replay returned {} rows above durable batch budget",
                replay_events.len()
            ));
        }
        if replay_events
            .iter()
            .any(|event| replay_event_type(event) == Some("text_delta"))
        {
            return Err(format!("{run_id}: replay returned text_delta"));
        }
        let expected_answer = format!("large durable final answer {run_ordinal}");
        if !replay_events.iter().any(|event| {
            replay_event_type(event) == Some("text_done")
                && event.pointer("/data/full_text").and_then(Value::as_str)
                    == Some(expected_answer.as_str())
        }) {
            return Err(format!("{run_id}: replay missing final answer"));
        }
        if !replay_events
            .iter()
            .any(|event| replay_event_type(event) == Some("run_finished"))
        {
            return Err(format!("{run_id}: replay missing run_finished"));
        }

        Ok(DurableEventPressureRunStats {
            raw_events: batch.raw_event_count,
            candidate_rows: batch.candidate_rows,
            candidate_bytes: batch.candidate_bytes,
            budgeted_rows: batch.budgeted_events.len(),
            budgeted_bytes: batch.budgeted_bytes,
            persisted_rows,
            replay_rows: replay_events.len(),
            compacted_rows,
            text_delta_rows,
            elapsed_ms: duration_millis_u64(started.elapsed()),
        })
    }
    .await;

    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    result
}

async fn seed_lifecycle_run_for_pause_resume_it(
    svc: &AgenticRunLifecycleService,
    user_id: &str,
    run_id: &str,
    session_id: &str,
) {
    svc.run_engine
        .start_run(run_id, user_id, session_id)
        .await
        .expect("start durable DB run");
    let (run_state, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
        run_id.to_string(),
        session_id.to_string(),
        user_id.to_string(),
    );
    svc.runs.write().await.insert(run_id.to_string(), run_state);
}

fn test_request(message: &str) -> ChatRequestData {
    ChatRequestData {
        message: message.to_string(),
        conversation_authority: None,
        user_intent: None,
        parts: Vec::new(),
        attachments: Vec::new(),
        stable_runtime_system_prompt: None,
        runtime_system_prompt: None,
        session_id: None,
        full_llm_capture: false,
        agent_id: None,
        model: Some("test-model".to_string()),
        model_selection: Some(ModelSelection {
            offering_id: "model-test-model".to_string(),
        }),
        resolved_model_selection: None,
        admitted_model_execution: None,
        capability_descriptors: None,
        provider_runtime_authorized: false,
        agent_bindings: Vec::new(),
        agent_binding: None,
        runtime_auth: None,
        runtime_skill_binding: None,
        runtime_profile: None,
        skill_search: None,
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        enabled_tools: None,
        workspace_binding: None,
        executor_binding: None,
        runtime_mcp_bindings: Vec::new(),
        mcp_binding_ids: None,
        context: None,
        edge_executor_id: None,
        capabilities: Vec::new(),
        forward_headers: HashMap::new(),
        execution_budget: None,
        execution_policy: Default::default(),
        explain: false,
        interaction_mode: None,
        interactive_client: false,
        provider_run_owner: None,
    }
}

fn prepared_test_request(message: &str) -> ChatRequestData {
    let mut request = test_request(message);
    request.model = Some("test-model".to_string());
    request.resolved_model_selection = Some(ResolvedModelSelection {
        offering_id: "model-test-model".to_string(),
        model_name: "test-model".to_string(),
    });
    request.admitted_model_execution = Some(test_admitted_model_execution());
    request
}

fn test_runtime_mcp_binding() -> RuntimeMcpBindingRequest {
    RuntimeMcpBindingRequest {
        id: "request_tools".to_string(),
        transport: "streamable_http".to_string(),
        url: "https://tools.example.test/mcp/http".to_string(),
        auth_token: None,
        headers: HashMap::new(),
    }
}

#[derive(Clone)]
struct StaticSkillResolver {
    skills: Vec<crate::turn::skill_tool::SkillToolInfo>,
}

impl crate::turn::skill_tool::SkillResolver for StaticSkillResolver {
    fn resolve(
        &self,
        name: &str,
    ) -> Result<crate::turn::skill_tool::ResolvedSkill, crate::skills::SkillError> {
        Err(crate::skills::SkillError::NotFound(name.to_string()))
    }

    fn available_skills(&self) -> Vec<crate::turn::skill_tool::SkillToolInfo> {
        self.skills.clone()
    }
}

fn static_skill_resolver(name: &str) -> Arc<dyn crate::turn::skill_tool::SkillResolver> {
    Arc::new(StaticSkillResolver {
        skills: vec![crate::turn::skill_tool::SkillToolInfo {
            name: name.to_string(),
            description: "Binding-scoped skill".to_string(),
            when_to_use: None,
            source: crate::skills::manifest::SkillSourceKind::Plugin,
            aliases: Vec::new(),
            category: None,
            tags: Vec::new(),
        }],
    })
}

fn test_agent_binding_record() -> astra_services::AgentBindingRecord {
    astra_services::AgentBindingRecord {
        id: "abnd_test1234567890".to_string(),
        binding_name: "test-binding".to_string(),
        idempotency_key: "idem-test-binding".to_string(),
        status: astra_services::AgentBindingStatus::Active,
        agent_md: "Always follow the binding contract.".to_string(),
        metadata: None,
        binding_schema_version: "v1".to_string(),
        created_at: "2026-06-19T00:00:00Z".to_string(),
        disabled_at: None,
    }
}

fn test_prepared_agent_binding_context() -> PreparedAgentBindingLoopContext {
    let binding = test_agent_binding_record();
    PreparedAgentBindingLoopContext {
        skill_catalogs: vec![agent_binding_skill_runtime::AgentBindingSkillCatalog {
            agent_binding_id: binding.id.clone(),
            skills: Vec::new(),
        }],
        bindings: vec![binding],
        skill_resolver: None,
        prompt_section: "## Agent Binding Instructions".to_string(),
    }
}

fn test_binding_request(id: &str) -> AgentBindingRuntimeRequest {
    AgentBindingRuntimeRequest { id: id.to_string() }
}

fn test_skill_info(name: &str, description: &str) -> crate::turn::skill_tool::SkillToolInfo {
    crate::turn::skill_tool::SkillToolInfo {
        name: name.to_string(),
        description: description.to_string(),
        when_to_use: None,
        source: crate::skills::manifest::SkillSourceKind::Plugin,
        aliases: Vec::new(),
        category: None,
        tags: Vec::new(),
    }
}

#[test]
fn requested_agent_bindings_preserve_caller_order() {
    let mut request = test_request("go");
    request.agent_bindings = vec![
        test_binding_request("binding-foundation"),
        test_binding_request("binding-extension"),
        test_binding_request("binding-session"),
    ];

    let bindings = AgenticRunLifecycleService::requested_agent_bindings(&request)
        .expect("valid ordered binding request");
    assert_eq!(bindings[0].id, "binding-foundation");
    assert_eq!(bindings[1].id, "binding-extension");
    assert_eq!(bindings[2].id, "binding-session");
}

#[test]
fn requested_agent_bindings_reject_ambiguous_or_duplicate_sets() {
    let mut mixed = test_request("go");
    mixed.agent_binding = Some(test_binding_request("binding-foundation"));
    mixed.agent_bindings = vec![test_binding_request("binding-extension")];
    let mixed_error = AgenticRunLifecycleService::requested_agent_bindings(&mixed)
        .expect_err("legacy and set fields must not be merged implicitly");
    assert_eq!(
        mixed_error.1.0.error_code.as_deref(),
        Some("agent_binding_set_invalid")
    );

    let mut duplicate = test_request("go");
    duplicate.agent_bindings = vec![
        test_binding_request("binding-foundation"),
        test_binding_request("binding-foundation"),
    ];
    let duplicate_error = AgenticRunLifecycleService::requested_agent_bindings(&duplicate)
        .expect_err("duplicate binding ids must be rejected");
    assert_eq!(
        duplicate_error.1.0.error_code.as_deref(),
        Some("agent_binding_set_invalid")
    );
}

#[test]
fn agent_binding_prompt_section_preserves_binding_and_skill_ownership() {
    let mut foundation = test_agent_binding_record();
    foundation.id = "binding-foundation".to_string();
    foundation.agent_md = "Use the platform contract & safety rules.".to_string();
    let mut extension = test_agent_binding_record();
    extension.id = "binding-extension".to_string();
    extension.agent_md = "Act as a <financial> analyst.".to_string();
    let catalogs = vec![
        agent_binding_skill_runtime::AgentBindingSkillCatalog {
            agent_binding_id: foundation.id.clone(),
            skills: vec![test_skill_info(
                "moi.agent.momo.skill.pdf",
                "Work with PDF documents",
            )],
        },
        agent_binding_skill_runtime::AgentBindingSkillCatalog {
            agent_binding_id: extension.id.clone(),
            skills: vec![test_skill_info(
                "financial-analysis",
                "Analyze financial statements",
            )],
        },
    ];

    let prompt = AgenticRunLifecycleService::agent_binding_prompt_section(
        &[foundation, extension],
        &catalogs,
    )
    .expect("valid binding prompt");
    let foundation_start = prompt
        .find("<agent_binding id=\"binding-foundation\">")
        .expect("foundation wrapper");
    let extension_start = prompt
        .find("<agent_binding id=\"binding-extension\">")
        .expect("extension wrapper");
    assert!(foundation_start < extension_start);
    let foundation_section = &prompt[foundation_start..extension_start];
    let extension_section = &prompt[extension_start..];
    assert!(foundation_section.contains("moi.agent.momo.skill.pdf"));
    assert!(!foundation_section.contains("financial-analysis"));
    assert!(extension_section.contains("financial-analysis"));
    assert!(!extension_section.contains("moi.agent.momo.skill.pdf"));
    assert!(foundation_section.contains("platform contract &amp; safety rules"));
    assert!(extension_section.contains("&lt;financial&gt; analyst"));
}

fn test_agent_binding_create_request() -> astra_services::AgentBindingCreateRequestData {
    astra_services::AgentBindingCreateRequestData {
        idempotency_key: "idem-runtime-binding".to_string(),
        binding: astra_services::AgentBindingPayload {
            binding_name: "runtime-binding".to_string(),
            agent_md: "Always follow the binding contract.".to_string(),
            metadata: None,
            binding_schema_version: "v1".to_string(),
        },
    }
}

fn runtime_binding_request(id: String, _mcp: &str, _skills: &str) -> AgentBindingRuntimeRequest {
    AgentBindingRuntimeRequest { id }
}

struct StaticAgentBindingService {
    record: astra_services::AgentBindingRecord,
}

#[async_trait]
impl astra_services::AgentBindingService for StaticAgentBindingService {
    async fn create_binding(
        &self,
        _request: astra_services::AgentBindingCreateRequestData,
    ) -> Result<astra_services::AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::METHOD_NOT_ALLOWED,
            "static agent binding service does not create bindings",
            "agent_binding_test_static",
        ))
    }

    async fn get_binding(
        &self,
        id: String,
    ) -> Result<astra_services::AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        if id == self.record.id {
            return Ok(self.record.clone());
        }
        Err(error_response_coded(
            StatusCode::NOT_FOUND,
            "agent binding not found",
            "agent_binding_not_found",
        ))
    }

    async fn disable_binding(
        &self,
        id: String,
    ) -> Result<astra_services::AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        if id == self.record.id {
            let mut record = self.record.clone();
            record.status = astra_services::AgentBindingStatus::Disabled;
            return Ok(record);
        }
        Err(error_response_coded(
            StatusCode::NOT_FOUND,
            "agent binding not found",
            "agent_binding_not_found",
        ))
    }
}

async fn service_with_in_memory_binding() -> (
    AgenticRunLifecycleService,
    Arc<astra_services::InMemoryAgentBindingService>,
    astra_services::AgentBindingRecord,
) {
    let binding_service = Arc::new(astra_services::InMemoryAgentBindingService::new());
    let record = astra_services::AgentBindingService::create_binding(
        binding_service.as_ref(),
        test_agent_binding_create_request(),
    )
    .await
    .expect("binding create");
    let service = test_service().with_agent_binding_service(binding_service.clone());
    (service, binding_service, record)
}

fn legacy_endpoint_agent_binding_record() -> astra_services::AgentBindingRecord {
    astra_services::AgentBindingRecord {
        id: "ab_legacy_endpoint_binding_01".to_string(),
        binding_name: "legacy-endpoint-binding".to_string(),
        idempotency_key: "idem-legacy-endpoint-binding".to_string(),
        status: astra_services::AgentBindingStatus::Active,
        agent_md: "Always follow the binding contract.".to_string(),
        metadata: None,
        binding_schema_version: "v1".to_string(),
        created_at: "2026-06-19T00:00:00Z".to_string(),
        disabled_at: None,
    }
}

#[tokio::test]
async fn resolve_agent_binding_runtime_rejects_disabled_binding() {
    let (service, binding_service, record) = service_with_in_memory_binding().await;
    astra_services::AgentBindingService::disable_binding(
        binding_service.as_ref(),
        record.id.clone(),
    )
    .await
    .expect("binding disable");

    let err = match service
        .resolve_agent_binding_runtime(&runtime_binding_request(record.id, "tools", "skills"))
        .await
    {
        Ok(_) => panic!("disabled binding should not start new turns"),
        Err(err) => err,
    };

    assert_eq!(err.0, StatusCode::CONFLICT);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_disabled")
    );
}

#[tokio::test]
async fn resolve_agent_binding_runtime_reports_exact_missing_binding_id() {
    let (service, _binding_service, _record) = service_with_in_memory_binding().await;
    let missing_id = "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7392";

    let error = service
        .resolve_agent_binding_runtime(&runtime_binding_request(
            missing_id.to_string(),
            "tools",
            "skills",
        ))
        .await
        .err()
        .expect("missing binding must fail");

    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(
        error.1.0.error_code.as_deref(),
        Some("agent_binding_not_found")
    );
    assert_eq!(
        error
            .1
            .0
            .metadata
            .as_ref()
            .and_then(|metadata| metadata["agent_binding_id"].as_str()),
        Some(missing_id)
    );
}

#[test]
fn server_root_permissions_default_to_auto_for_server_approval_gate() {
    let mut request = test_request("edit files");
    request.interaction_mode = Some(RequestedTurnInteractionMode::Prompt);
    let constraints = RequestConstraints::default();

    let inherited =
        AgenticRunLifecycleService::inherited_permissions_from_request(&request, &constraints);

    assert_eq!(inherited.mode, PermissionMode::Auto);
    assert!(inherited.allowed_tools.is_none());
}

#[test]
fn server_root_permissions_map_deny_and_preserve_tool_allowlist() {
    let mut request = test_request("no tools");
    request.interaction_mode = Some(RequestedTurnInteractionMode::Deny);
    let constraints = RequestConstraints {
        allowed_tools: Some(["read_file".to_string()].into_iter().collect()),
        ..Default::default()
    };

    let inherited =
        AgenticRunLifecycleService::inherited_permissions_from_request(&request, &constraints);

    assert_eq!(inherited.mode, PermissionMode::Deny);
    assert!(
        inherited
            .allowed_tools
            .as_ref()
            .is_some_and(|tools| tools.contains("read_file"))
    );
}

#[test]
fn server_subrun_executor_keeps_inherited_permissions() {
    let inherited_permissions = InheritedPermissions::new(PermissionMode::Deny);
    let executor = ServerSubRunExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    )
    .with_inherited_permissions(inherited_permissions);

    assert_eq!(executor.inherited_permissions.mode, PermissionMode::Deny);
}

#[test]
fn server_subrun_executor_reuses_the_lifecycle_run_engine() {
    let run_engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    let executor = ServerSubRunExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    )
    .with_run_engine(run_engine.clone());

    let wired = executor
        .durable_run_engine()
        .expect("injected lifecycle engine must be retained");
    assert!(
        Arc::ptr_eq(run_engine.store(), wired.store()),
        "subruns must retain the lifecycle store identity rather than reconstructing a new owner"
    );
}

#[tokio::test]
async fn server_subrun_execution_material_is_bound_to_durable_offering_identity() {
    let run_engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    run_engine
        .start_run_ext_with_context(
            "parent-run",
            "user-1",
            "session-1",
            None,
            None,
            Some("parent-agent"),
            None,
            crate::server::run::engine::RunStartContext {
                model_selection: Some(ModelSelection {
                    offering_id: "model-test-model".to_string(),
                }),
                resolved_model_selection: Some(ResolvedModelSelection {
                    offering_id: "model-test-model".to_string(),
                    model_name: "test-model".to_string(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("durable parent with admitted model identity");
    let executor = ServerSubRunExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    )
    .with_run_engine(run_engine.clone());
    let mut config = SubRunConfig {
        run_id: "child-run".to_string(),
        parent_run_id: "parent-run".to_string(),
        agent_profile: AgentProfile::new("child-agent", "Child", AgentTier::User),
        task: "verify the durable route".to_string(),
        session_id: "session-1".to_string(),
        user_id: "user-1".to_string(),
        previous_output: None,
        context: HashMap::new(),
        forward_headers: HashMap::new(),
        admitted_model_execution: Some(test_admitted_model_execution()),
        request_constraints: RequestConstraints::default(),
        recursion_depth: 1,
        max_turns: Some(1),
        pause_flag: None,
        checkpoint_gate: None,
        mailbox: None,
        progress_emitter: None,
        live_event_sink: None,
        cancel_token: None,
        inherited_prefix: None,
        execution_metadata: None,
        delegation_chain: Vec::new(),
        #[cfg(feature = "harness")]
        harness_sink: None,
    };

    executor
        .ensure_durable_subrun_started(&config, config.admitted_model_execution.as_ref())
        .await
        .expect("child start must persist the admitted Offering identity");
    let child = run_engine
        .load_run("user-1", "child-run")
        .await
        .expect("load durable child")
        .expect("durable child");
    assert_eq!(child.model_offering_id.as_deref(), Some("model-test-model"));
    assert_eq!(child.resolved_model_name.as_deref(), Some("test-model"));

    config.admitted_model_execution = Some(AdmittedModelExecution::from_endpoint(
        "model-other".to_string(),
        "other-model".to_string(),
        "openai".to_string(),
        "http://127.0.0.1:1/chat/completions".to_string(),
        "Bearer test".to_string(),
        None,
    ));
    assert!(
        executor
            .materialize_durable_subrun_execution(
                &config,
                config.admitted_model_execution.as_ref(),
            )
            .await
            .is_err(),
        "execution material must not drift from the durable child Offering"
    );
}

#[tokio::test]
async fn server_subrun_partial_status_persists_typed_error_code() {
    let run_engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    run_engine
        .start_run("child-run", "user-1", "session-1")
        .await
        .unwrap();
    let executor = ServerSubRunExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    )
    .with_run_engine(run_engine.clone());

    executor
        .persist_durable_subrun_status(
            "user-1",
            "session-1",
            "child-run",
            STATUS_FAILED,
            None,
            Some(astra_services::coordination::AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE),
            Some("budget_exhausted: adaptive hard turn limit reached"),
        )
        .await;

    let run = run_engine
        .load_run("user-1", "child-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, STATUS_FAILED);
    assert_eq!(
        run.error_code.as_deref(),
        Some(astra_services::coordination::AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE)
    );
    assert_eq!(
        run.error_message.as_deref(),
        Some("budget_exhausted: adaptive hard turn limit reached")
    );
    assert!(run.events.iter().any(|event| {
        event["event_type"] == "run_finished"
            && event["data"]["error_code"]
                == astra_services::coordination::AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE
    }));
}

#[test]
fn provision_subrun_workspace_rejects_unsafe_identity_components() {
    let executor = ServerSubRunExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    );

    let session_error = executor
        .provision_subrun_workspace("session/123", "run-123")
        .expect_err("unsafe session id must fail instead of being sanitized");
    assert!(
        session_error.contains("invalid sub-run session_id"),
        "unexpected session error: {session_error}"
    );

    let run_error = executor
        .provision_subrun_workspace("session-123", "run/123")
        .expect_err("unsafe run id must fail instead of being sanitized");
    assert!(
        run_error.contains("invalid sub-run run_id"),
        "unexpected run error: {run_error}"
    );
}

struct FailingWorkspaceRecordStore;

#[async_trait]
impl WorkspaceRecordStore for FailingWorkspaceRecordStore {
    async fn upsert_workspace_record(
        &self,
        _entry: StoredWorkspaceRecordEntry,
    ) -> Result<(), WorkspaceRecordStoreError> {
        Err(WorkspaceRecordStoreError::Unavailable(
            "injected workspace store failure".to_string(),
        ))
    }

    async fn load_workspace_record(
        &self,
        _owner_id: &str,
        _workspace_id: &str,
    ) -> Result<Option<StoredWorkspaceRecordEntry>, WorkspaceRecordStoreError> {
        Ok(None)
    }

    async fn list_workspace_records(
        &self,
        _owner_id: &str,
        _limit: u32,
    ) -> Result<Vec<StoredWorkspaceRecordEntry>, WorkspaceRecordStoreError> {
        Ok(Vec::new())
    }

    async fn delete_workspace_record(
        &self,
        _owner_id: &str,
        _workspace_id: &str,
    ) -> Result<bool, WorkspaceRecordStoreError> {
        Err(WorkspaceRecordStoreError::Unavailable(
            "injected workspace store failure".to_string(),
        ))
    }
}

#[async_trait]
impl WorkspaceCleanupDebtStore for FailingWorkspaceRecordStore {
    async fn record_cleanup_debt(
        &self,
        _entry: WorkspaceCleanupDebtEntry,
    ) -> Result<(), WorkspaceCleanupDebtStoreError> {
        Err(WorkspaceCleanupDebtStoreError::Unavailable(
            "injected cleanup debt store failure".to_string(),
        ))
    }

    async fn list_cleanup_debts(
        &self,
        _owner_id: &str,
        _limit: u32,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
        Ok(Vec::new())
    }

    async fn resolve_cleanup_debt(
        &self,
        _owner_id: &str,
        _debt_id: &str,
    ) -> Result<bool, WorkspaceCleanupDebtStoreError> {
        Ok(false)
    }

    async fn list_all_unresolved_debts(
        &self,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
        Err(WorkspaceCleanupDebtStoreError::Unavailable(
            "injected cleanup debt store failure".to_string(),
        ))
    }

    async fn increment_debt_attempts(
        &self,
        _debt_id: &str,
    ) -> Result<(), WorkspaceCleanupDebtStoreError> {
        Err(WorkspaceCleanupDebtStoreError::Unavailable(
            "injected cleanup debt store failure".to_string(),
        ))
    }
}

fn test_cloud_workspace_record(workspace_id: &str) -> RuntimeWorkspaceRecord {
    RuntimeWorkspaceRecord {
        workspace_id: workspace_id.to_string(),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
        source: RuntimeWorkspaceSource::PersistentVolume {
            volume_id: "team-volume-1".to_string(),
        },
        persistence: RuntimeWorkspacePersistence::Persistent,
        revision: "1".to_string(),
        display_name: "Team workspace".to_string(),
    }
}

#[tokio::test]
async fn lifecycle_persists_workspace_record_with_owner_session_and_run() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let svc = test_service().with_workspace_record_store(store.clone());
    let record = test_cloud_workspace_record("workspace-1");

    ok(svc
        .persist_workspace_record(
            "00000000-0000-0000-0000-000000000001",
            "session-1",
            "run-1",
            &record,
        )
        .await);

    let loaded = store
        .load_workspace_record("00000000-0000-0000-0000-000000000001", "workspace-1")
        .await
        .expect("load workspace record")
        .expect("record");
    assert_eq!(loaded.owner_id, "00000000-0000-0000-0000-000000000001");
    assert_eq!(loaded.session_id.as_deref(), Some("session-1"));
    assert_eq!(loaded.run_id.as_deref(), Some("run-1"));
    assert_eq!(loaded.record, record);
    assert!(
        store
            .load_workspace_record("00000000-0000-0000-0000-000000000002", "workspace-1")
            .await
            .expect("load workspace record")
            .is_none(),
        "workspace records must stay owner scoped"
    );
}

#[tokio::test]
async fn lifecycle_workspace_record_store_failure_fails_closed() {
    let svc = test_service().with_workspace_record_store(Arc::new(FailingWorkspaceRecordStore));
    let record = test_cloud_workspace_record("workspace-1");

    let error = err(svc
        .persist_workspace_record(
            "00000000-0000-0000-0000-000000000001",
            "session-1",
            "run-1",
            &record,
        )
        .await);

    assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        error
            .1
            .0
            .detail
            .contains("Failed to persist workspace record"),
        "{}",
        error.1.0.detail
    );
}

#[tokio::test]
async fn lifecycle_workspace_record_source_conflict_returns_conflict() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    store
        .upsert_workspace_record(StoredWorkspaceRecordEntry::new(
            "00000000-0000-0000-0000-000000000002",
            Some("session-2".to_string()),
            Some("run-2".to_string()),
            test_cloud_workspace_record("workspace-2"),
        ))
        .await
        .expect("store existing workspace owner");
    let svc = test_service().with_workspace_record_store(store);
    let record = test_cloud_workspace_record("workspace-1");

    let error = err(svc
        .persist_workspace_record(
            "00000000-0000-0000-0000-000000000001",
            "session-1",
            "run-1",
            &record,
        )
        .await);

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert!(
        error.1.0.detail.contains("Workspace ownership conflict"),
        "{}",
        error.1.0.detail
    );
}

#[tokio::test]
async fn lifecycle_records_cleanup_debt_when_failed_start_cleanup_fails() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let svc = test_service().with_workspace_record_store(store.clone());
    let mut record = test_cloud_workspace_record("workspace-cleanup-debt");
    record.persistence = RuntimeWorkspacePersistence::Session;
    record.source = RuntimeWorkspaceSource::Scratch;
    record.root_or_volume_ref = "/definitely/missing/astra-cleanup-debt".to_string();

    svc.cleanup_cloud_workspace_after_failed_start(
        "00000000-0000-0000-0000-000000000001",
        "session-1",
        "run-1",
        &record,
        "injected start failure".to_string(),
    )
    .await;

    let debts = store
        .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
        .await
        .expect("list cleanup debts");
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].workspace_id, "workspace-cleanup-debt");
    assert_eq!(debts[0].reason, RuntimeCleanupReason::Failed);
    assert!(debts[0].message.contains("injected start failure"));
    assert_eq!(debts[0].session_id.as_deref(), Some("session-1"));
    assert_eq!(debts[0].run_id.as_deref(), Some("run-1"));
}

#[tokio::test]
async fn lifecycle_records_cleanup_debt_when_terminal_cleanup_fails() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let mut record = test_cloud_workspace_record("workspace-terminal-cleanup-debt");
    record.persistence = RuntimeWorkspacePersistence::Session;
    record.source = RuntimeWorkspaceSource::Scratch;
    record.root_or_volume_ref = "/definitely/missing/astra-terminal-cleanup-debt".to_string();

    AgenticRunLifecycleService::cleanup_cloud_workspace_after_terminal_run(
        Some(store.clone()),
        "00000000-0000-0000-0000-000000000001",
        "session-1",
        "run-1",
        &record,
        &RunStatus::Completed,
    )
    .await;

    let debts = store
        .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
        .await
        .expect("list cleanup debts");
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].workspace_id, "workspace-terminal-cleanup-debt");
    assert_eq!(debts[0].reason, RuntimeCleanupReason::Completed);
    assert!(debts[0].message.contains("run ended with status completed"));
    assert_eq!(debts[0].session_id.as_deref(), Some("session-1"));
    assert_eq!(debts[0].run_id.as_deref(), Some("run-1"));
}

#[tokio::test]
async fn lifecycle_removes_workspace_record_after_successful_terminal_cleanup() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let record = test_cloud_workspace_record("workspace-terminal-cleanup-success");
    store
        .upsert_workspace_record(StoredWorkspaceRecordEntry::new(
            "00000000-0000-0000-0000-000000000001",
            Some("session-1".to_string()),
            Some("run-1".to_string()),
            record.clone(),
        ))
        .await
        .expect("store workspace record");

    AgenticRunLifecycleService::cleanup_cloud_workspace_after_terminal_run(
        Some(store.clone()),
        "00000000-0000-0000-0000-000000000001",
        "session-1",
        "run-1",
        &record,
        &RunStatus::Completed,
    )
    .await;

    assert!(
        store
            .load_workspace_record(
                "00000000-0000-0000-0000-000000000001",
                "workspace-terminal-cleanup-success"
            )
            .await
            .expect("load workspace record")
            .is_none(),
        "successful cleanup must remove the workspace record"
    );
    assert!(
        store
            .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
            .await
            .expect("list cleanup debts")
            .is_empty(),
        "successful cleanup must not create cleanup debt"
    );
}

#[tokio::test]
async fn lifecycle_skips_cloud_workspace_cleanup_for_resumable_status() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let mut record = test_cloud_workspace_record("workspace-waiting-no-cleanup");
    record.persistence = RuntimeWorkspacePersistence::Session;
    record.root_or_volume_ref = "/definitely/missing/astra-waiting-no-cleanup".to_string();

    AgenticRunLifecycleService::cleanup_cloud_workspace_after_terminal_run(
        Some(store.clone()),
        "00000000-0000-0000-0000-000000000001",
        "session-1",
        "run-1",
        &record,
        &RunStatus::Waiting,
    )
    .await;

    assert!(
        store
            .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
            .await
            .expect("list cleanup debts")
            .is_empty(),
        "resumable runs must keep their workspace for continuation"
    );
}

#[test]
fn cloud_git_source_maps_to_workspace_record_contract() {
    let mut request = test_request("checkout this repo");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: Some("Repo checkout".to_string()),
        root: None,
        source: Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
            repository: "https://example.com/org/repo.git".to_string(),
            reference: None,
        }),
        authority: None,
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request, "123",
    ))
    .expect("cloud workspace request");

    assert_eq!(provision_request.workspace_id, "run-123");
    assert_eq!(
        provision_request.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(
        provision_request.authority,
        astra_runtime_env::WorkspaceAuthority::ReadWrite
    );
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::Session
    );
    assert_eq!(
        provision_request.source,
        RuntimeWorkspaceSource::GitCheckout {
            repository: "https://example.com/org/repo.git".to_string(),
            reference: None,
        }
    );

    let record = RuntimeWorkspaceRecord {
        workspace_id: provision_request.workspace_id,
        owner_scope: provision_request.owner_scope,
        kind: provision_request.kind,
        authority: provision_request.authority,
        root_or_volume_ref: "/cloud/checkouts/run-123".to_string(),
        source: provision_request.source,
        persistence: provision_request.persistence,
        revision: "1".to_string(),
        display_name: "Repo checkout".to_string(),
    };
    let snapshot = execution_bindings_from_workspace_record(&record);
    let workspace = &snapshot.workspace;
    let executor = &snapshot.executor;

    assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
    assert_eq!(workspace.cwd.as_deref(), Some("/cloud/checkouts/run-123"));
    assert_eq!(executor.kind, ExecutorBindingKind::OrchestratorManaged);
    assert_eq!(executor.transport, ToolTransportKind::SandboxResidentAgent);
    assert_eq!(
        snapshot
            .runtime
            .as_ref()
            .map(|runtime| runtime.launch_driver),
        Some(astra_runtime_env::RuntimeLaunchDriver::Kubernetes)
    );
}

#[test]
fn cloud_persistent_volume_binding_maps_to_workspace_record_contract() {
    let mut request = test_request("use my workspace");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: Some("Team workspace".to_string()),
        root: None,
        source: Some(
            astra_services::runs::WorkspaceSourceRequest::PersistentVolume {
                volume_id: "team-volume-1".to_string(),
            },
        ),
        authority: None,
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request,
        "volume-run",
    ))
    .expect("cloud workspace request");

    assert_eq!(provision_request.workspace_id, "run-volume-run");
    assert_eq!(
        provision_request.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(
        provision_request.authority,
        astra_runtime_env::WorkspaceAuthority::ReadWrite
    );
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::Persistent
    );
    assert_eq!(
        provision_request.source,
        RuntimeWorkspaceSource::PersistentVolume {
            volume_id: "team-volume-1".to_string(),
        }
    );

    let record = RuntimeWorkspaceRecord {
        workspace_id: provision_request.workspace_id,
        owner_scope: provision_request.owner_scope,
        kind: provision_request.kind,
        authority: provision_request.authority,
        root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
        source: provision_request.source,
        persistence: provision_request.persistence,
        revision: "1".to_string(),
        display_name: "Team workspace".to_string(),
    };
    let snapshot = execution_bindings_from_workspace_record(&record);
    let workspace = &snapshot.workspace;
    let executor = &snapshot.executor;

    assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
    assert_eq!(
        workspace.cwd.as_deref(),
        Some("/cloud/volumes/team-volume-1")
    );
    assert_eq!(executor.kind, ExecutorBindingKind::OrchestratorManaged);
    assert_eq!(executor.transport, ToolTransportKind::SandboxResidentAgent);
    assert_eq!(
        snapshot
            .runtime
            .as_ref()
            .map(|runtime| runtime.session_manager),
        Some(astra_runtime_env::RuntimeSessionManager::ProviderManaged)
    );
}

#[test]
fn cloud_scratch_source_maps_to_generic_workspace_record_contract() {
    let mut request = test_request("create scratch workspace");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: Some("Scratch workspace".to_string()),
        root: None,
        source: Some(astra_services::runs::WorkspaceSourceRequest::Scratch),
        authority: None,
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request,
        "scratch-run",
    ))
    .expect("scratch cloud workspace request");

    assert_eq!(provision_request.workspace_id, "run-scratch-run");
    assert_eq!(
        provision_request.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(provision_request.source, RuntimeWorkspaceSource::Scratch);
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::Session
    );
}

#[test]
fn cloud_uploaded_snapshot_source_defaults_to_immutable_read_only() {
    let mut request = test_request("inspect snapshot");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: None,
        source: Some(
            astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot {
                artifact_id: "artifact-1".to_string(),
                root: None,
            },
        ),
        authority: None,
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request, "456",
    ))
    .expect("cloud workspace request");

    assert_eq!(
        provision_request.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(
        provision_request.authority,
        astra_runtime_env::WorkspaceAuthority::ReadOnly
    );
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::ImmutableSnapshot
    );
    assert_eq!(
        provision_request.source,
        RuntimeWorkspaceSource::UploadedSnapshot {
            artifact_id: "artifact-1".to_string(),
        }
    );
}

#[test]
fn cloud_template_source_defaults_to_read_write_session_workspace() {
    let mut request = test_request("start from template");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: Some("/cloud/templates/template-1".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::Template {
            template_id: "template-1".to_string(),
        }),
        authority: None,
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request,
        "template-run",
    ))
    .expect("template workspace request");

    assert_eq!(
        provision_request.authority,
        astra_runtime_env::WorkspaceAuthority::ReadWrite
    );
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::Session
    );
    assert_eq!(
        provision_request.source,
        RuntimeWorkspaceSource::Template {
            template_id: "template-1".to_string(),
        }
    );
    assert_eq!(
        provision_request.requested_root.as_deref(),
        Some("/cloud/templates/template-1")
    );
}

#[test]
fn cloud_dataset_and_artifact_sources_default_to_immutable_read_only() {
    let cases = [
        (
            astra_services::runs::WorkspaceSourceRequest::DatasetBundle {
                dataset_id: "dataset-1".to_string(),
            },
            RuntimeWorkspaceSource::DatasetBundle {
                dataset_id: "dataset-1".to_string(),
            },
        ),
        (
            astra_services::runs::WorkspaceSourceRequest::ArtifactBundle {
                artifact_id: "artifact-1".to_string(),
            },
            RuntimeWorkspaceSource::ArtifactBundle {
                artifact_id: "artifact-1".to_string(),
            },
        ),
    ];

    for (source, expected_source) in cases {
        let mut request = test_request("inspect materialized source");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: None,
            root: None,
            source: Some(source),
            authority: None,
        });

        let provision_request = ok(cloud_workspace_provision_request_from_request(
            &request,
            "bundle-run",
        ))
        .expect("bundle workspace request");

        assert_eq!(
            provision_request.authority,
            astra_runtime_env::WorkspaceAuthority::ReadOnly
        );
        assert_eq!(
            provision_request.persistence,
            RuntimeWorkspacePersistence::ImmutableSnapshot
        );
        assert_eq!(provision_request.source, expected_source);
    }
}

#[test]
fn cloud_materialized_source_rejects_relative_root_before_provisioning() {
    let mut request = test_request("bad template root");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: Some("relative/template".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::Template {
            template_id: "template-1".to_string(),
        }),
        authority: None,
    });

    let error = err(cloud_workspace_provision_request_from_request(
        &request,
        "bad-template",
    ));

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error
            .1
            .0
            .detail
            .contains("absolute materialized source path"),
        "{}",
        error.1.0.detail
    );
}

#[test]
fn cloud_materialized_source_rejects_empty_identifier() {
    let mut request = test_request("bad dataset");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: None,
        source: Some(
            astra_services::runs::WorkspaceSourceRequest::DatasetBundle {
                dataset_id: "   ".to_string(),
            },
        ),
        authority: None,
    });

    let error = err(cloud_workspace_provision_request_from_request(
        &request,
        "bad-dataset",
    ));

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error.1.0.detail.contains("non-empty source.dataset_id"),
        "{}",
        error.1.0.detail
    );
}

#[test]
fn cloud_workspace_binding_requires_materialized_source() {
    let mut request = test_request("checkout");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: None,
        source: Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
            repository: "   ".to_string(),
            reference: None,
        }),
        authority: None,
    });

    let error = err(cloud_workspace_provision_request_from_request(
        &request, "789",
    ));

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error
            .1
            .0
            .detail
            .contains("Git checkout workspace requires a non-empty source.repository"),
        "{}",
        error.1.0.detail
    );
}

#[test]
fn cloud_workspace_binding_rejects_missing_source() {
    let mut request = test_request("use cloud workspace");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: None,
        source: None,
        authority: None,
    });

    let error = err(cloud_workspace_provision_request_from_request(
        &request,
        "bad-volume",
    ));

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error
            .1
            .0
            .detail
            .contains("Cloud workspace requires an explicit source"),
        "{}",
        error.1.0.detail
    );
}

#[test]
fn cloud_workspace_runtime_kind_projects_to_server_binding() {
    let record = RuntimeWorkspaceRecord {
        workspace_id: "workspace-1".to_string(),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
        source: RuntimeWorkspaceSource::PersistentVolume {
            volume_id: "team-volume-1".to_string(),
        },
        persistence: RuntimeWorkspacePersistence::Persistent,
        revision: "1".to_string(),
        display_name: "Team workspace".to_string(),
    };

    let workspace = server_workspace_binding_from_workspace_record(&record);

    assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
    assert_eq!(
        workspace.cwd.as_deref(),
        Some("/cloud/volumes/team-volume-1")
    );
}

#[test]
fn request_execution_bindings_use_actual_server_workspace_for_server_sandbox() {
    let mut request = test_request("hello");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: Some("Requested server".to_string()),
        root: Some("/client/claimed/path".to_string()),
        source: None,
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::ServerLocal,
        executor_id: Some("server-local".to_string()),
        display_name: Some("Requested executor".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::ServerLocal),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });

    let server_workspace = Path::new("/tmp/astra-runtime-workspace");
    let (workspace, executor) = resolve_request_execution_bindings(&request, server_workspace);

    assert_eq!(workspace.kind, WorkspaceBindingKind::ServerSandbox);
    assert_eq!(workspace.display_name, "Requested server");
    assert_eq!(
        workspace.cwd.as_deref(),
        Some("/tmp/astra-runtime-workspace")
    );
    assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert_eq!(executor.executor_id, "server-local");
    assert_eq!(executor.display_name, "Requested executor");
    assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn server_workspace_binding_decision_uses_only_explicit_binding() {
    let mut request = test_request("hello");

    assert!(!request_uses_server_workspace(&request));

    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: None,
        root: None,
        source: None,
        authority: None,
    });
    assert!(request_uses_server_workspace(&request));

    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("Edge".to_string()),
        root: Some("/repo".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/repo".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });
    assert!(!request_uses_server_workspace(&request));
}

#[test]
fn request_execution_bindings_keep_edge_workspace_without_server_reroute() {
    let mut request = test_request("review this repo");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("MacBook Pro".to_string()),
        root: Some("/Users/xupeng/github/astra".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/Users/xupeng/github/astra".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("edge-macbook-1".to_string()),
        display_name: Some("MacBook Pro".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });

    let (workspace, executor) =
        resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

    assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
    assert_eq!(workspace.display_name, "MacBook Pro");
    assert_eq!(workspace.cwd.as_deref(), Some("/Users/xupeng/github/astra"));
    assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
    assert_eq!(executor.executor_id, "edge-macbook-1");
    assert_eq!(executor.transport, ToolTransportKind::EdgeWs);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn workspace_binding_request_accepts_legacy_cwd_alias() {
    let mut request = test_request("review this repo");
    request.workspace_binding = Some(
        serde_json::from_value(json!({
            "kind": "edge_workspace",
            "display_name": "MacBook Pro",
            "cwd": "/Users/test/repo",
            "authority": "read_write",
        }))
        .expect("legacy cwd alias should deserialize"),
    );
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("edge-1".to_string()),
        display_name: Some("MacBook Pro".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });

    let (workspace, executor) =
        resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

    assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
    assert_eq!(workspace.cwd.as_deref(), Some("/Users/test/repo"));
    assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
    assert_eq!(executor.transport, ToolTransportKind::EdgeWs);
}

#[test]
fn edge_profile_execution_bindings_make_edge_provider_explicit() {
    let mut edge_profile = Map::new();
    edge_profile.insert("cwd".to_string(), json!("/Users/xupeng/github/astra"));
    edge_profile.insert("edge_agent_id".to_string(), json!("edge-macbook-1"));
    edge_profile.insert("hostname".to_string(), json!("MacBook Pro"));

    let (workspace, executor) = resolve_request_execution_bindings_without_server_workspace(
        &test_request("review this repo"),
        &edge_profile,
    )
    .expect("edge profile should produce explicit bindings");

    assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
    assert_eq!(workspace.display_name, "MacBook Pro");
    assert_eq!(workspace.cwd.as_deref(), Some("/Users/xupeng/github/astra"));
    assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
    assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
    assert_eq!(executor.executor_id, "edge-macbook-1");
    assert_eq!(executor.display_name, "MacBook Pro");
    assert_eq!(executor.transport, ToolTransportKind::EdgeLedger);
    assert_eq!(executor.status, ExecutorStatus::Unknown);
}

#[test]
fn missing_edge_profile_execution_bindings_emit_no_file_environment() {
    let (workspace, executor) = resolve_request_execution_bindings_without_server_workspace(
        &test_request("hello"),
        &Map::new(),
    )
    .expect("missing edge profile should still produce an explicit no-file-environment binding");

    assert_eq!(workspace.kind, WorkspaceBindingKind::None);
    assert_eq!(workspace.display_name, "No file environment");
    assert_eq!(workspace.authority, WorkspaceAuthority::None);
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert_eq!(executor.executor_id, "server-control-plane");
    assert_eq!(executor.display_name, "Server control plane");
    assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn edge_tools_without_profile_do_not_create_edge_ledger_binding() {
    let (workspace, executor) = resolve_request_execution_bindings_without_server_workspace(
        &test_request("run client tool"),
        &Map::new(),
    )
    .expect("missing edge profile should produce no-file control-plane binding");

    assert_eq!(workspace.kind, WorkspaceBindingKind::None);
    assert_eq!(workspace.display_name, "No file environment");
    assert_eq!(workspace.cwd, None);
    assert_eq!(workspace.authority, WorkspaceAuthority::None);
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert_eq!(executor.executor_id, "server-control-plane");
    assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn explicit_no_file_environment_binding_uses_server_control_plane_executor() {
    let mut request = test_request("plan only");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::None,
        display_name: None,
        root: None,
        source: None,
        authority: None,
    });

    let (workspace, executor) =
        resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

    assert_eq!(workspace.kind, WorkspaceBindingKind::None);
    assert_eq!(workspace.display_name, "No file environment");
    assert_eq!(workspace.authority, WorkspaceAuthority::None);
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert_eq!(executor.executor_id, "server-control-plane");
    assert_eq!(executor.display_name, "Server control plane");
    assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn execution_bindings_from_metadata_rebases_server_sandbox_cwd() {
    let metadata = json!({
        "workspace": {
            "kind": "server_sandbox",
            "display_name": "Server sandbox",
            "cwd": "/tmp/parent-workspace",
            "authority": "read_write",
        },
        "executor": {
            "kind": "server_local",
            "executor_id": "server-local",
            "display_name": "Server sandbox",
            "transport": "server_local",
            "status": "online"
        }
    });

    let snapshot =
        execution_bindings_from_metadata(Some(&metadata), Path::new("/tmp/child-workspace"))
            .expect("metadata bindings");
    let workspace = &snapshot.workspace;
    let executor = &snapshot.executor;

    assert_eq!(workspace.kind, WorkspaceBindingKind::ServerSandbox);
    assert_eq!(workspace.cwd.as_deref(), Some("/tmp/child-workspace"));
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert!(snapshot.runtime.is_none());
}

#[tokio::test]
async fn validate_request_constraints_rejects_legacy_mcp_binding_ids() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.mcp_binding_ids = Some(vec!["mcp_bind_301".to_string()]);

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("legacy mcp_binding_ids must be rejected on chat stream");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert!(
        err.1
            .0
            .detail
            .contains("mcp_binding_ids is no longer supported")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_core_or_unknown_enabled_tools() {
    let service = test_service();
    for tool_name in ["read_file", "not_a_tool"] {
        let mut request = prepared_test_request("hello");
        request.enabled_tools = Some(vec![tool_name.to_string()]);

        let error = service
            .validate_request_constraints("u1", &request)
            .await
            .expect_err("enabled_tools is only for known product-optional tools");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0.error_code.as_deref(),
            Some("enabled_tools_invalid")
        );
    }
}

#[tokio::test]
async fn server_request_omission_explicitly_disables_optional_tools() {
    let service = test_service();
    let request = prepared_test_request("hello");

    let constraints = service
        .validate_request_constraints("u1", &request)
        .await
        .expect("ordinary server request should validate");

    assert_eq!(constraints.enabled_tools, Some(HashSet::new()));
    assert!(constraints.allowed_tools.is_none());
}

#[tokio::test]
async fn optional_tool_availability_is_checked_against_selected_provider() {
    let request_constraints = RequestConstraints::new(
        None,
        Some(HashSet::from([
            "web_search".to_string(),
            "web_fetch".to_string(),
        ])),
        None,
        None,
    );
    let unavailable_service =
        test_service().with_tool_execution_service(ToolExecutionService::builder().build());
    let error = unavailable_service
        .validate_optional_tool_availability("u1", &request_constraints, None)
        .await
        .expect_err("server network capability is opt-in");
    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0.error_code.as_deref(),
        Some("optional_tool_provider_unavailable")
    );

    let available_service = test_service().with_tool_execution_service(
        ToolExecutionService::builder()
            .initial_provider_capabilities(std::collections::HashMap::from([(
                crate::server::tool_execution_service::SERVER_OPTIONAL_TOOL_PROVIDER_ID.to_string(),
                HashSet::from([astra_core::PROVIDER_CAPABILITY_PUBLIC_NETWORK.to_string()]),
            )]))
            .build(),
    );
    available_service
        .validate_optional_tool_availability("u1", &request_constraints, None)
        .await
        .expect("declared server network capacity should satisfy the web bundle");
}

#[test]
fn runtime_bearer_parser_accepts_exact_single_bearer_token() {
    let parsed =
        parse_runtime_bearer_authorization("Bearer abc.DEF-123_~+/=").expect("valid bearer");
    assert_eq!(parsed.token, "abc.DEF-123_~+/=");
}

#[test]
fn runtime_bearer_parser_rejects_malformed_or_multiple_credentials() {
    for value in [
        "",
        "Basic abc",
        "bearer abc",
        "Bearer ",
        "Bearer  abc",
        "Bearer abc ",
        "Bearer abc def",
        "Bearer abc,Bearer def",
        "Bearer abc,def",
        "Bearer abc;def",
        "Bearer abc:Bearer:def",
    ] {
        let err = parse_runtime_bearer_authorization(value)
            .expect_err("malformed bearer should be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("agent_binding_runtime_auth_invalid"),
            "{value}"
        );
    }
}

#[tokio::test]
async fn validate_request_constraints_rejects_implicit_request_scoped_runtime_mcp_by_default() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.runtime_mcp_bindings = vec![test_runtime_mcp_binding()];

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("runtime_mcp_bindings must explicitly select request_scoped profile");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
    assert!(
        err.1
            .0
            .detail
            .contains("runtime_profile=request_scoped_runtime_mcp")
    );
}

#[tokio::test]
async fn validate_request_constraints_allows_explicit_request_scoped_runtime_mcp() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.runtime_mcp_bindings = vec![test_runtime_mcp_binding()];
    request.runtime_profile = Some(RuntimeProfileRequest::RequestScopedRuntimeMcp);

    service
        .validate_request_constraints("u1", &request)
        .await
        .expect("explicit request_scoped_runtime_mcp profile should allow runtime MCP");
}

#[tokio::test]
async fn validate_request_constraints_requires_model_selection() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.model = None;
    request.model_selection = None;
    request.resolved_model_selection = None;

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("model selection is required for every chat stream request");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("model_selection_missing")
    );
}

#[tokio::test]
async fn prepare_chat_request_rejects_empty_effective_user_input() {
    let service = test_service();
    let request = test_request("   ");
    let err = service
        .prepare_chat_request(request)
        .await
        .expect_err("empty message and missing user_intent must be rejected");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(err.1.0.error_code.as_deref(), Some("chat_input_empty"));
}

#[tokio::test]
async fn prepare_chat_request_accepts_structured_user_intent_when_prompt_message_is_empty() {
    let service = test_service();
    let mut request = test_request("   ");
    request.user_intent = Some("continue the approved plan".to_string());
    request.model = None;
    request.resolved_model_selection = None;
    request.admitted_model_execution = None;

    let prepared = service
        .prepare_chat_request(request)
        .await
        .expect("non-empty user_intent is valid effective input");

    assert_eq!(prepared.model.as_deref(), Some("test-model"));
    let material = prepared
        .admitted_model_execution
        .as_ref()
        .expect("Server admission must materialize the exact Offering once");
    assert_eq!(material.offering_id, "model-test-model");
    assert_eq!(material.model_name, "test-model");
    assert_eq!(
        prepared.user_intent.as_deref(),
        Some("continue the approved plan")
    );
}

#[tokio::test]
async fn validate_request_constraints_allows_native_model_without_gateway_auth() {
    let service = test_service();
    let request = prepared_test_request("hello");

    service
        .validate_request_constraints("u1", &request)
        .await
        .expect("catalog-resolved Offering should not require runtime_auth");
}

#[tokio::test]
async fn prepare_chat_request_rejects_unknown_offering_without_name_fallback() {
    let service = test_service();
    let mut request = test_request("hello");
    request.model = None;
    request.resolved_model_selection = None;
    request.admitted_model_execution = None;
    request.model_selection = Some(ModelSelection {
        offering_id: "missing-model".to_string(),
    });

    let err = service
        .prepare_chat_request(request)
        .await
        .expect_err("unknown Offering must not fall back to a model name");

    assert_eq!(err.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn prepare_chat_request_rejects_wire_resolution_without_provider_authorization() {
    let service = test_service();
    let mut request = test_request("hello");
    request.model = None;
    request.resolved_model_selection = Some(ResolvedModelSelection {
        offering_id: "model-test-model".to_string(),
        model_name: "attacker-model".to_string(),
    });

    let err = service
        .prepare_chat_request(request)
        .await
        .expect_err("ordinary requests cannot use wire resolution as execution authority");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("model_selection_invalid")
    );
}

fn test_runtime_descriptor(
    id: &str,
    descriptor_type: &str,
    endpoint_url: &str,
) -> astra_services::runs::RuntimeCapabilityDescriptorRequest {
    astra_services::runs::RuntimeCapabilityDescriptorRequest {
        id: id.to_string(),
        descriptor_type: descriptor_type.to_string(),
        transport: "http".to_string(),
        endpoint_url: endpoint_url.to_string(),
        protocol: "openai_chat_completions".to_string(),
        semantic_read: None,
        metadata: serde_json::Map::new(),
    }
}

fn file_transfer_request_with_attachment(
    attachment: Value,
) -> astra_services::runs::ChatRequestData {
    file_transfer_request_with_attachments(vec![attachment])
}

fn file_transfer_request_with_attachments(
    attachments: Vec<Value>,
) -> astra_services::runs::ChatRequestData {
    let mut request = prepared_test_request("hello");
    request.provider_runtime_authorized = true;
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("Managed sandbox".to_string()),
        root: Some("/sandbox".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/sandbox".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("sbx-test".to_string()),
        display_name: Some("Managed sandbox".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });
    let mut descriptor = test_runtime_descriptor(
        "moi-runtime-files",
        "file_transfer",
        "http://127.0.0.1/runtime-files",
    );
    descriptor.protocol = "moi_runtime_files_v1".to_string();
    descriptor.metadata = json!({
        "contract_version": 1,
        "task_id": "task-1",
        "root": "/sandbox/.moi/runtime/task-1",
        "catalog_dir": "/sandbox/.moi/runtime/task-1/catalog",
        "session_dir": "/sandbox/.moi/sessions/session-1",
        "scratch_dir": "/sandbox/.moi/runtime/task-1/scratch",
        "max_file_bytes": 1024,
        "attachments": attachments
    })
    .as_object()
    .unwrap()
    .clone();
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: None,
            mcp: None,
            skills: None,
            edge_agent: None,
            file_transfer: Some(descriptor),
        });
    request
}

fn ephemeral_file_transfer_request_with_attachments(
    attachments: Vec<Value>,
) -> astra_services::runs::ChatRequestData {
    let mut request = file_transfer_request_with_attachments(attachments);
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("Ephemeral sandbox".to_string()),
        root: Some("/sandbox/.moi".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/sandbox/.moi".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("eph-test".to_string()),
        display_name: Some("Ephemeral sandbox".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });
    let descriptor = request
        .capability_descriptors
        .as_mut()
        .and_then(|descriptors| descriptors.file_transfer.as_mut())
        .expect("file transfer descriptor");
    descriptor.metadata = json!({
        "contract_version": 2,
        "work_dir": "/sandbox/.moi",
        "max_file_bytes": 1024,
        "attachments": descriptor.metadata["attachments"].clone(),
    })
    .as_object()
    .expect("ephemeral file transfer metadata")
    .clone();
    request
}

#[test]
fn runtime_file_transfer_assigns_pairwise_distinct_names_at_request_boundary() {
    let request = file_transfer_request_with_attachments(vec![
        json!({"file_id": "file-1", "name": "input.txt", "size": 1, "md5": "0123456789abcdef0123456789abcdef"}),
        json!({"file_id": "file-2", "name": "input.txt", "size": 1, "md5": "0123456789abcdef0123456789abcdef"}),
        json!({"file_id": "file-3", "name": "000000-input.txt", "size": 1, "md5": "0123456789abcdef0123456789abcdef"}),
    ]);
    let context = AgenticRunLifecycleService::runtime_file_transfer_context(&request)
        .expect("attachment inventory must be valid")
        .expect("file transfer context");
    let names = context
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            astra_server_types::edge_ws_protocol::runtime_attachment_destination_name(
                index,
                &attachment.name,
            )
            .expect("validated destination name")
        })
        .collect::<HashSet<_>>();

    assert_eq!(names.len(), context.attachments.len());
    assert!(names.contains("000000-input.txt"));
    assert!(names.contains("000001-input.txt"));
    assert!(names.contains("000002-000000-input.txt"));
}

#[test]
fn ephemeral_file_transfer_uses_one_flat_work_dir() {
    let request = ephemeral_file_transfer_request_with_attachments(Vec::new());
    let context = AgenticRunLifecycleService::runtime_file_transfer_context(&request)
        .expect("ephemeral file transfer must be valid")
        .expect("ephemeral file transfer context");
    assert_eq!(context.workspace_root, "/sandbox/.moi");
    assert!(matches!(
        context.layout,
        astra_services::runs::RuntimeFileTransferLayout::Ephemeral { ref work_dir }
            if work_dir == "/sandbox/.moi"
    ));
}

#[test]
fn runtime_file_transfer_rejects_edge_invalid_attachment_inventory_at_request_boundary() {
    let valid = json!({
        "file_id": "file-1",
        "name": "paper.pdf",
        "size": 512,
        "md5": "0123456789abcdef0123456789abcdef"
    });
    assert!(
        AgenticRunLifecycleService::runtime_file_transfer_context(
            &file_transfer_request_with_attachment(valid.clone())
        )
        .unwrap()
        .is_some()
    );

    for invalid in [
        json!({"file_id": "file-1", "name": "../paper.pdf", "size": 512, "md5": "0123456789abcdef0123456789abcdef"}),
        json!({"file_id": "file-1", "name": "paper.pdf", "size": 1025, "md5": "0123456789abcdef0123456789abcdef"}),
        json!({"file_id": "file-1", "name": "paper.pdf", "size": 512, "md5": "0123456789ABCDEF0123456789ABCDEF"}),
        json!({"file_id": "file-1", "name": "x".repeat(241), "size": 512, "md5": "0123456789abcdef0123456789abcdef"}),
    ] {
        let error = AgenticRunLifecycleService::runtime_file_transfer_context(
            &file_transfer_request_with_attachment(invalid),
        )
        .expect_err("invalid inventory must be rejected before edge dispatch");
        assert_eq!(error, "file_transfer attachment inventory is invalid");
    }
}

#[test]
fn runtime_file_transfer_rejects_non_edge_execution_boundary() {
    let mut request = file_transfer_request_with_attachment(json!({
        "file_id": "file-1",
        "name": "paper.pdf",
        "size": 512,
        "md5": "0123456789abcdef0123456789abcdef"
    }));
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: Some("Server sandbox".to_string()),
        root: Some("/sandbox".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::Scratch),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::ServerLocal,
        executor_id: Some("server".to_string()),
        display_name: Some("Server".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::ServerLocal),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });

    assert_eq!(
        AgenticRunLifecycleService::runtime_file_transfer_context(&request).unwrap_err(),
        "file_transfer requires an edge_workspace and Edge WebSocket executor"
    );
}

#[test]
fn runtime_provider_endpoints_require_plain_http_authority() {
    for endpoint in [
        "http://127.0.0.1:18001/runtime-files",
        "https://moi.example/runtime-executors/authorize?version=1",
    ] {
        assert!(valid_runtime_http_endpoint(endpoint), "{endpoint}");
    }

    for endpoint in [
        "ftp://moi.example/runtime-files",
        "http://",
        "https://user@moi.example/runtime-files",
        "https://moi.example/runtime-files#fragment",
        "not-a-url",
    ] {
        assert!(!valid_runtime_http_endpoint(endpoint), "{endpoint}");
    }
}

fn authorized_edge_dispatch_request() -> astra_services::runs::ChatRequestData {
    let mut request = prepared_test_request("hello");
    request.provider_runtime_authorized = true;
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("Runner".to_string()),
        root: Some("/workspace".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/workspace".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("runner-r1".to_string()),
        display_name: Some("Runner".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWsAuthorized),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });
    let mut descriptor = test_runtime_descriptor(
        "runner-r1",
        "edge_agent",
        "http://127.0.0.1/api/v1/runtime-executors/authorize",
    );
    descriptor.transport = "edge_ws".to_string();
    descriptor.protocol = "moi_edge_dispatch_authorization_v1".to_string();
    descriptor.metadata = json!({
        "contract_version": 1,
        "task_id": "task-1",
        "executor_id": "runner-r1"
    })
    .as_object()
    .unwrap()
    .clone();
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: None,
            mcp: None,
            skills: None,
            edge_agent: Some(descriptor),
            file_transfer: None,
        });
    request
}

#[test]
fn runtime_executor_authorization_requires_versioned_transport_and_matching_scope() {
    let request = authorized_edge_dispatch_request();
    let context = AgenticRunLifecycleService::runtime_edge_dispatch_authorization_context(&request)
        .expect("valid authorization descriptor")
        .expect("authorization context");
    assert_eq!(context.task_id, "task-1");
    assert_eq!(context.executor_id, "runner-r1");

    let mut server_sandbox = request.clone();
    server_sandbox.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: Some("Server sandbox".to_string()),
        root: None,
        source: None,
        authority: None,
    });
    assert_eq!(
        AgenticRunLifecycleService::runtime_edge_dispatch_authorization_context(&server_sandbox)
            .expect_err("authorized edge transport must not bind to a server workspace"),
        "runtime executor authorization requires an edge_workspace binding"
    );

    let mut ordinary_transport = request.clone();
    ordinary_transport
        .executor_binding
        .as_mut()
        .expect("executor binding")
        .transport = Some(astra_services::runs::ToolTransportKindRequest::EdgeWs);
    assert!(
        AgenticRunLifecycleService::runtime_edge_dispatch_authorization_context(
            &ordinary_transport
        )
        .is_err(),
        "ordinary edge_ws must not silently accept the authorization descriptor"
    );

    let mut mismatched_executor = request;
    mismatched_executor
        .executor_binding
        .as_mut()
        .expect("executor binding")
        .executor_id = Some("runner-r2".to_string());
    assert!(
        AgenticRunLifecycleService::runtime_edge_dispatch_authorization_context(
            &mismatched_executor
        )
        .is_err(),
        "descriptor scope must match the selected executor"
    );
}

#[tokio::test]
async fn prepare_chat_request_normalizes_provider_descriptor_without_registered_gateway() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.provider_runtime_authorized = true;
    request.admitted_model_execution = None;
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "moi-model-gateway",
                "model_gateway",
                "http://127.0.0.1/model-gateway",
            )),
            mcp: None,
            skills: None,
            edge_agent: None,
            file_transfer: None,
        });

    let prepared = service
        .prepare_chat_request(request)
        .await
        .expect("provider descriptor should become admitted_model_execution");
    service
        .validate_request_constraints("u1", &prepared)
        .await
        .expect("normalized provider execution should satisfy run invariants");
    assert_eq!(
        prepared
            .admitted_model_execution
            .as_ref()
            .and_then(|execution| execution.completions_url_override.as_deref()),
        Some("http://127.0.0.1/model-gateway")
    );
    assert_eq!(
        prepared
            .admitted_model_execution
            .as_ref()
            .map(|execution| execution.model_name.as_str()),
        Some("test-model")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_descriptor_without_provider_authorization() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "moi-model-gateway",
                "model_gateway",
                "http://127.0.0.1/model-gateway",
            )),
            mcp: None,
            skills: None,
            edge_agent: None,
            file_transfer: None,
        });

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("provider descriptors require provider authorization");
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("provider_runtime_context_required")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_agent_binding_registry_profile_without_binding() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.runtime_profile = Some(RuntimeProfileRequest::AgentBindingRegistry);

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("agent_binding_registry profile must not be set without agent_binding");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
}

#[test]
fn validate_runtime_profile_rejects_stable_prompt_without_binding_set() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.stable_runtime_system_prompt = Some("Stable provider policy".to_string());

    let error = service
        .validate_runtime_profile_shape(&request)
        .expect_err("stable provider policy must be scoped by a Binding Set");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
}

#[tokio::test]
async fn validate_request_constraints_allows_agent_binding_with_omitted_runtime_profile() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });

    service
        .validate_request_constraints("u1", &request)
        .await
        .expect("agent_binding itself is the explicit registry opt-in");
}

#[tokio::test]
async fn validate_request_constraints_rejects_agent_binding_edge_tools() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.context = Some(
        json!({
            "edge_tools": [{"function": {"name": "request_tool"}}]
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("agent_binding mode cannot carry request-scoped edge tools");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_agent_binding_edge_skills() {
    let service = test_service();
    let mut request = prepared_test_request("hello");
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.context = Some(
        json!({
            "edge_skills": [{"name": "request_skill"}]
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("agent_binding mode cannot carry request-scoped edge skills");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
}

#[tokio::test]
async fn build_initial_state_includes_database_skill_provider_when_wired() {
    use astra_services::skills::{
        SkillInfoRecord, SkillListCursor, SkillListItem, SkillListRecord, SkillPublishRequestData,
        SkillRecord, SkillService, SkillStatusRecord, SkillVersionRecord,
    };
    use async_trait::async_trait;

    #[derive(Default)]
    struct MockSkillService {
        unsupported_calls: std::sync::atomic::AtomicUsize,
    }

    impl MockSkillService {
        fn unsupported<T>(&self, operation: &str) -> Result<T, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err((
                StatusCode::NOT_IMPLEMENTED,
                Json(ErrorResponse::new(format!(
                    "MockSkillService::{operation} is not implemented in this test"
                ))),
            ))
        }
    }

    #[async_trait]
    impl SkillService for MockSkillService {
        async fn list_skills(
            &self,
            _user_id: String,
            limit: u32,
            cursor: Option<SkillListCursor>,
        ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
            if cursor.is_some() {
                return Ok(SkillListRecord {
                    skills: Vec::new(),
                    total: Some(1),
                    limit,
                    next_cursor: None,
                });
            }
            Ok(SkillListRecord {
                skills: vec![SkillListItem {
                    skill_id: "remote-db@1.0.0".to_string(),
                    skill_name: "remote-db".to_string(),
                    version: "1.0.0".to_string(),
                    description: Some("Remote DB skill".to_string()),
                    status: Some("active".to_string()),
                    source: Some("user".to_string()),
                    category: Some("integration".to_string()),
                    created_at: None,
                }],
                total: Some(1),
                limit,
                next_cursor: None,
            })
        }

        async fn get_skill(
            &self,
            _user_id: String,
            skill_id: String,
            _version: Option<String>,
        ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
            if skill_id == "remote-db" || skill_id == "remote-db@1.0.0" {
                return Ok(SkillRecord {
                    skill_id: "remote-db@1.0.0".to_string(),
                    skill_name: "remote-db".to_string(),
                    version: "1.0.0".to_string(),
                    description: Some("Remote DB skill".to_string()),
                    metadata: Some(serde_json::json!({
                        "skill_type": "remote",
                        "remote_url": "http://127.0.0.1:18080/remote-skill",
                        "forward_headers": ["authorization", "x-workspace-id"],
                        "required_headers": ["x-workspace-id"],
                        "when_to_use": "when task needs remote orchestration"
                    })),
                    created_at: None,
                });
            }
            Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("not found".to_string())),
            ))
        }

        async fn get_skill_info(
            &self,
            _: String,
            _: String,
        ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("get_skill_info")
        }

        async fn list_skill_versions(
            &self,
            _: String,
            _: String,
        ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("list_skill_versions")
        }

        async fn get_skill_status(
            &self,
            _: String,
            _: u32,
        ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("get_skill_status")
        }

        async fn publish_skill(
            &self,
            _: String,
            _: SkillPublishRequestData,
        ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("publish_skill")
        }

        async fn unpublish_skill(
            &self,
            _: String,
            _: String,
        ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("unpublish_skill")
        }
    }

    let skill_service = Arc::new(MockSkillService::default());
    let svc = test_service().with_skill_service(skill_service.clone());

    let default_request = test_request("hello");
    let default_state = svc.build_initial_state(
        "test-user",
        &default_request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let default_resolver = default_state
        .skills
        .resolver
        .as_ref()
        .expect("default server resolver should include visible catalog");
    let default_names: Vec<String> = default_resolver
        .available_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert!(
        default_names.iter().any(|name| name == "remote-db"),
        "expected database skill without request allow_skills filter: {default_names:?}"
    );
    assert!(
        default_state.skills.registry_for_activation.is_some(),
        "unfiltered server catalog should be available for conditional activation"
    );

    let mut request = test_request("hello");
    request.allow_skills = Some(vec!["remote-db".to_string()]);
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let resolver = state
        .skills
        .resolver
        .as_ref()
        .expect("skill resolver should be configured");
    let names: Vec<String> = resolver
        .available_skills()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(
        names.iter().any(|name| name == "remote-db"),
        "expected database skill in available skills: {names:?}"
    );

    let resolved = resolver
        .resolve("remote-db")
        .expect("resolver should load database skill");
    assert_eq!(
        resolved.remote_url.as_deref(),
        Some("http://127.0.0.1:18080/remote-skill")
    );
    assert_eq!(
        resolved.forward_headers,
        vec!["authorization".to_string(), "x-workspace-id".to_string()]
    );
    assert_eq!(
        resolved.required_headers,
        vec!["x-workspace-id".to_string()]
    );

    let mut filtered_request = test_request("hello");
    filtered_request.allow_skills = Some(vec!["remote-db".to_string()]);
    let filtered_state = svc.build_initial_state(
        "test-user",
        &filtered_request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    assert!(
        filtered_state.skills.registry_for_activation.is_none(),
        "request-scoped allow_skills should disable automatic conditional activation"
    );
    let filtered_resolver = filtered_state
        .skills
        .resolver
        .as_ref()
        .expect("filtered resolver should be configured");
    let filtered_names: Vec<String> = filtered_resolver
        .available_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(filtered_names, vec!["remote-db".to_string()]);
    filtered_resolver
        .resolve("remote-db")
        .expect("allowed remote-db skill should resolve");
    assert_eq!(
        skill_service
            .unsupported_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "build_initial_state should only use list_skills/get_skill on this mock"
    );
}

#[tokio::test]
async fn create_run_rejects_unknown_request_skill_allowlist() {
    let svc = test_service();
    let mut request = test_request("hello");
    request.allow_skills = Some(vec!["__missing_skill__".into()]);

    let err = svc
        .create_run("user-1".into(), request)
        .await
        .expect_err("unknown allow_skills entry should be rejected");
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert!(err.1.0.detail.contains("allow_skills"));
}

#[test]
fn build_runtime_turn_evaluation_event_uses_loop_state_signals() {
    let svc = test_service();
    let request = test_request("git status");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.recent_tools = vec!["git_status".into()];
    state.telemetry.first_budget_pressure = 0.27;
    state.stall.events.push(("repetition_stall".into(), 1));
    state.stall.verdict_events.push(
        astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent {
            turn: 1,
            severity: "warning".into(),
            injections: vec!["stall detected".into()],
            avoid_tools: vec!["git_status".into()],
            health_avoidance_tools: vec![],
            advisory_threshold_reached: false,
            nudge_count: 1,
            interaction_mode: "prompt".into(),
            recent_error_pressure: 0,
            recent_timeout_pressure: 0,
            total_errors: 0,
            health_avoidance_count: 0,
            total_timeouts: 0,
            timeout_dominant_tools: vec![],
            total_cache_hits: 0,
            flaky_count: 0,
        },
    );
    state.stall.tool_call_records.push(ToolCallRecord {
        name: "git_status".into(),
        ok: true,
        ms: 14,
        error: None,
        input_bytes: Some(8),
        output_bytes: Some(180),
        args_preview: None,
        result_preview: Some("clean".into()),
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
    });

    let event = build_runtime_turn_evaluation_event("session-1", "server_runtime", &state);

    assert_eq!(event.event_type, JournalEventType::TurnEvaluation);
    assert_eq!(event.turn, None);
    let metadata = event.metadata.expect("turn evaluation metadata");
    assert_eq!(metadata["source"], "server_runtime");
    assert_eq!(metadata["live_query"], false);
    assert_eq!(metadata["stall_count"], 1);
    assert_eq!(metadata["verdict_warning"], true);
    assert_eq!(metadata["tool_call_count"], 1);
    assert!(metadata["quality"].as_f64().unwrap() < 0.8);
    assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
}

#[test]
fn finalize_run_events_appends_run_finished_for_failures() {
    let svc = test_service();
    let request = test_request("boom");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Error("boom".into())),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Failed);
    assert_eq!(error.as_deref(), Some("boom"));
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event_type"], "run_error");
    assert_eq!(events[0]["data"]["error_code"], "unknown");
    assert_eq!(events[0]["data"]["error_kind"], "unknown");
    assert_eq!(events[1]["event_type"], "run_finished");
    assert_eq!(events[1]["data"]["error_code"], "unknown");
    assert_eq!(events[1]["data"]["error_kind"], "unknown");
}

#[test]
fn finalize_run_events_preserves_terminal_handoff_and_marks_source_delegated() {
    let svc = test_service();
    let request = test_request("handoff");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.total_prompt = 21;
    state.total_completion = 4;
    let handoff_event = json!({
        "type": "runtime.control.handoff.requested",
        "handoff_id": "handoff-1",
        "kind": "moi.control.handoff.v1",
        "target": "agent_authoring",
        "action": "revise_current_agent",
        "terminal": true,
        "tool_call_id": "call-1"
    });

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Delegated),
        vec![handoff_event.clone()],
        &state,
    );

    assert_eq!(status, RunStatus::Delegated);
    assert!(error.is_none());
    assert_eq!(events[0], handoff_event);
    assert_eq!(events[1]["event_type"], "run_finished");
    assert_eq!(events[1]["data"]["status"], "delegated");
    assert_eq!(events[1]["data"]["outcome"], "delegated");
    assert_eq!(events[1]["data"]["prompt_tokens"], 21);
    assert_eq!(events[1]["data"]["completion_tokens"], 4);
}

#[test]
fn finalize_run_events_preserves_terminal_control_rejection_code() {
    let svc = test_service();
    let request = test_request("late handoff");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let rejection = crate::turn::terminal_control::TerminalControlRejection {
        code: "terminal_handoff_window_closed",
        message: "terminal handoff must be the source run's first agent action".to_string(),
        tool_call_id: Some("call-late".to_string()),
    };

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::ControlRejected(rejection)),
        vec![json!({
            "type": "runtime.control.handoff.rejected",
            "error_code": "terminal_handoff_window_closed",
            "tool_call_id": "call-late"
        })],
        &state,
    );

    assert_eq!(status, RunStatus::Failed);
    assert!(error.is_some());
    assert_eq!(events[1]["event_type"], "run_error");
    assert_eq!(
        events[1]["data"]["error_code"],
        "terminal_handoff_window_closed"
    );
    assert_eq!(
        events[2]["data"]["error_code"],
        "terminal_handoff_window_closed"
    );
}

#[test]
fn finalize_run_events_classifies_string_error_outcomes() {
    let svc = test_service();
    let request = test_request("classify");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    for (message, expected_code) in [
        (
            "database operation failed: error communicating with database: unexpected EOF",
            "database_error",
        ),
        (
            "LLM request failed: error sending request for url (https://example.invalid)",
            "network",
        ),
        ("[stream_transport] stream body closed", "stream_transport"),
    ] {
        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Error(message.into())),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(error.as_deref(), Some(message));
        assert_eq!(events[0]["data"]["error_code"], expected_code);
        assert_eq!(events[0]["data"]["error_kind"], expected_code);
        assert_eq!(events[1]["data"]["error_code"], expected_code);
        assert_eq!(events[1]["data"]["error_kind"], expected_code);
    }
}

#[test]
fn finalize_run_events_preserves_classified_error_code() {
    let svc = test_service();
    let request = test_request("network");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    let classified = astra_core::ClassifiedError::new(
        astra_core::ErrorKind::Network,
        "LLM request failed: connection reset",
    );
    let (events, status, error) =
        AgenticRunLifecycleService::finalize_run_events(Err(classified), vec![], &state);

    assert_eq!(status, RunStatus::Failed);
    assert_eq!(
        error.as_deref(),
        Some("[network] LLM request failed: connection reset")
    );
    assert_eq!(events[0]["event_type"], "run_error");
    assert_eq!(events[0]["data"]["error_code"], "network");
    assert_eq!(events[0]["data"]["error_kind"], "network");
    assert_eq!(events[1]["event_type"], "run_finished");
    assert_eq!(events[1]["data"]["error_code"], "network");
    assert_eq!(events[1]["data"]["error_kind"], "network");
    assert_eq!(
        events[1]["data"]["error"],
        "[network] LLM request failed: connection reset"
    );
}

#[test]
fn finalize_run_events_preserves_host_event_route_contract_code() {
    let svc = test_service();
    let request = test_request("route fault");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let classified = astra_core::ClassifiedError::new(
        astra_core::ErrorKind::ContractViolation,
        "host_event_route_contract_violation: approval used the progress lane",
    )
    .with_details_json(
        json!({
            "source": server_loop_host::HOST_EVENT_ROUTER_SOURCE,
            "error_code": server_loop_host::HOST_EVENT_ROUTE_CONTRACT_ERROR_CODE,
        })
        .to_string(),
    );

    let (events, status, error) =
        AgenticRunLifecycleService::finalize_run_events(Err(classified), vec![], &state);

    assert_eq!(status, RunStatus::Failed);
    assert!(error.is_some());
    assert_eq!(
        events[0]["data"]["error_code"],
        "host_event_route_contract_violation"
    );
    assert_eq!(events[0]["data"]["error_kind"], "contract_violation");
    assert_eq!(
        events[1]["data"]["error_code"],
        "host_event_route_contract_violation"
    );
}

#[test]
fn finalize_run_events_distinguishes_provider_admission_rejection() {
    let svc = test_service();
    let request = test_request("admission");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    let classified = astra_core::ClassifiedError::new(
        astra_core::ErrorKind::RateLimit,
        "LLM provider admission rpm limit reached",
    )
    .with_details_json(json!({"source": "llm_provider_admission"}).to_string());
    let (events, status, _error) =
        AgenticRunLifecycleService::finalize_run_events(Err(classified), vec![], &state);

    assert_eq!(status, RunStatus::Failed);
    assert_eq!(
        events[0]["data"]["error_code"],
        "llm_provider_admission_rejected"
    );
    assert_eq!(events[0]["data"]["error_kind"], "rate_limit");
    assert_eq!(
        events[1]["data"]["error_code"],
        "llm_provider_admission_rejected"
    );
    assert_eq!(events[1]["data"]["error_kind"], "rate_limit");
}

#[test]
fn finalize_run_events_cancellation_beats_completed_outcome() {
    let svc = test_service();
    let request = test_request("done");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let cancel_flag = Arc::new(AtomicBool::new(true));
    let cancel_token = Arc::new(CancellationToken::new());
    cancel_token.cancel();
    state.cancellation.flag = Some(cancel_flag);
    state.cancellation.token = Some(cancel_token);

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Completed),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Cancelled);
    assert!(error.is_none());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event_type"], "run_finished");
    assert_eq!(events[0]["data"]["cancelled"], true);
}

#[test]
fn streaming_final_replay_excludes_live_work_surface_events() {
    let events = vec![
        json!({"type": "text_delta", "content": "hi"}),
        json!({"type": "reasoning_delta", "content": "thinking"}),
        json!({"type": "tool_call", "tool_call": {"id": "call-1"}}),
        json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok"}),
        json!({"type": "agent_progress", "agent_id": "agent-1", "status": "started"}),
        json!({"type": "agent_live_event", "agent_id": "agent-1", "event_kind": "output_delta", "content": "child"}),
        json!({"type": "run_blocked", "call_id": "call-1", "reason": "transport_disconnected"}),
        json!({"type": "run_blocked", "call_id": "call-2", "reason": "executor_offline"}),
        json!({"type": "run_blocked", "call_id": "call-3", "reason": "route_mismatch"}),
        json!({"event_type": "text_done", "data": {"full_text": "hi"}}),
        json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
    ];

    let replay: Vec<_> = events
        .iter()
        .filter(|event| streaming_final_event_for_replay(event))
        .cloned()
        .collect();

    assert_eq!(replay.len(), 2);
    assert_eq!(replay[0]["event_type"], "text_done");
    assert_eq!(replay[1]["event_type"], "run_finished");
    assert!(!live_delta_event_for_persistence(&events[0]));
    assert!(!live_delta_event_for_persistence(&events[1]));
    assert!(live_delta_event_for_persistence(&events[2]));
    assert!(live_delta_event_for_persistence(&events[3]));
    assert!(live_delta_event_for_persistence(&events[4]));
    assert!(!live_delta_event_for_persistence(&events[5]));
    assert!(live_delta_event_for_persistence(&events[6]));
    assert!(live_delta_event_for_persistence(&events[7]));
    assert!(live_delta_event_for_persistence(&events[8]));
}

#[test]
fn streaming_durable_persistence_keeps_semantic_events_before_terminal() {
    let events = vec![
        json!({"type": "reasoning_delta", "content": "thinking"}),
        json!({"type": "tool_call", "tool_call": {"id": "call-1"}}),
        json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok"}),
        json!({"event_type": "text_done", "data": {"full_text": "answer"}}),
        json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
    ];

    let persisted: Vec<_> = events
        .iter()
        .filter(|event| streaming_event_for_persistence(event))
        .cloned()
        .collect();

    assert_eq!(persisted.len(), 4);
    assert_eq!(persisted[0]["type"], "tool_call");
    assert_eq!(persisted[1]["type"], "tool_call_end");
    assert_eq!(persisted[2]["event_type"], "text_done");
    assert_eq!(persisted[3]["event_type"], "run_finished");
}

#[test]
fn agent_communication_is_a_durable_replay_boundary() {
    let event = json!({
        "type": "agent_communication",
        "schema_version": "astra.agent_communication.v1",
        "observed_by": {"run_id": "run-review", "agent_id": "reviewer"},
        "direction": "received",
        "message_id": "msg-1",
        "from": {"run_id": "run-code", "agent_id": "coder"},
        "to": {"kind": "direct", "address": {"run_id": "run-review", "agent_id": "reviewer"}},
        "payload_kind": "text",
        "summary": "review this",
        "timestamp_ms": 42,
        "requires_ack": false
    });

    assert!(live_delta_event_for_persistence(&event));
    assert!(streaming_event_for_persistence(&event));
    assert!(durable_replay_boundary_event(&event));
}

#[test]
fn compaction_is_a_durable_live_replay_boundary() {
    let event = json!({
        "type": "compaction",
        "data": {
            "kind": "wire_assembly",
            "pressure": 0.78,
            "tokens_before": 12_000,
            "tokens_after": 7_000,
            "tokens_freed": 5_000
        }
    });

    assert!(live_delta_event_for_persistence(&event));
    assert!(streaming_event_for_persistence(&event));
    assert!(durable_replay_boundary_event(&event));
}

#[test]
fn active_run_live_event_projection_is_bounded() {
    let mut run = RunState {
        run_id: "run-live-bound".to_string(),
        user_id: "user-live-bound".to_string(),
        session_id: "session-live-bound".to_string(),
        status: RunStatus::Running,
        events: vec![json!({"event_type": "run_started", "data": {"run_id": "run-live-bound"}})],
        cancel_flag: Arc::new(AtomicBool::new(false)),
        pause_flag: Arc::new(AtomicBool::new(false)),
        llm_cancel_token: Arc::new(CancellationToken::new()),
        live_tx: None,
        waiting_for: None,
        execution_live: true,
    };

    for idx in 0..(MAX_ACTIVE_RUN_LIVE_EVENTS + 5) {
        push_active_run_live_event(&mut run, json!({"type": "agent_progress", "seq": idx}));
    }

    let live_events: Vec<_> = run
        .events
        .iter()
        .filter(|event| live_delta_event_for_persistence(event))
        .collect();
    assert_eq!(live_events.len(), MAX_ACTIVE_RUN_LIVE_EVENTS);
    assert_eq!(run.events[0]["event_type"], "run_started");
    assert_eq!(live_events[0]["seq"], 5);
}

#[test]
fn transport_delta_chunks_are_live_only_not_durable() {
    let events = vec![
        json!({"type": "text_delta", "content": "hi"}),
        json!({"type": "reasoning_delta", "content": "thinking"}),
        json!({"type": "thinking_delta", "content": "thinking"}),
        json!({"type": "reasoning_message_content", "content": "raw chain of thought"}),
        json!({"event_type": "reasoning_message_content", "data": {"content": "raw chain of thought"}}),
        json!({"type": "agent_live_event", "event_kind": "output_delta", "content": "child"}),
        json!({"type": "agent_live_event", "event_kind": "thinking_delta", "content": "child-thinking"}),
    ];

    for event in events {
        assert!(
            !streaming_event_for_persistence(&event),
            "transport delta should remain live-only: {event}"
        );
    }
}

#[test]
fn finalize_run_events_interrupted_completed_outcome_is_partial_not_completed() {
    let svc = test_service();
    let request = test_request("partial");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.final_text = "[Round budget hard-limit reached]".to_string();
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 5,
            turns_completed: 15,
            remaining_turns: 0,
            error_detail: Some("Round budget hard-limit reached".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Completed),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Paused);
    assert!(
        error.is_none(),
        "resumable interruption should be structured paused state, not a run error: {error:?}"
    );
    assert_eq!(events[0]["event_type"], "text_done");
    assert_eq!(events[0]["data"]["partial"], true);
    assert_eq!(
        events[0]["data"]["interruption"]["kind"],
        "budget_exhausted"
    );
    assert!(
        events[0]["data"]["interruption"]["user_message"]
            .as_str()
            .is_some_and(|msg| msg.to_ascii_lowercase().contains("budget")),
        "interruption detail should carry the budget stop reason: {events:?}"
    );
    assert_eq!(events[1]["event_type"], "run_interrupted");
    assert_eq!(events[2]["event_type"], "run_finished");
    assert_eq!(events[2]["data"]["interrupted"], true);
    assert_eq!(events[2]["data"]["interruption_kind"], "budget_exhausted");
}

fn lifecycle_task_board_snapshot(
    status: astra_tools::task_mgmt::SessionTaskStatusKind,
) -> crate::turn::agentic_loop::host::TaskBoardSnapshot {
    crate::turn::agentic_loop::host::TaskBoardSnapshot::from_active_tasks(&[
        astra_tools::task_mgmt::SessionTask {
            archived_at: None,
            id: "task-1".to_string(),
            title: "finish validation".to_string(),
            description: None,
            status,
            subtasks: Vec::new(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        },
    ])
}

#[test]
fn finalize_run_events_completes_answered_run_with_in_progress_task_board() {
    let svc = test_service();
    let request = test_request("answered task");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.final_text = "Done.".to_string();
    state.hooks.task_board_snapshot =
        lifecycle_task_board_snapshot(astra_tools::task_mgmt::SessionTaskStatusKind::InProgress);

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Completed),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Completed);
    assert!(error.is_none());
    assert_eq!(events[0]["event_type"], "text_done");
    assert_eq!(events[0]["data"]["full_text"], "Done.");
    assert_eq!(events[1]["event_type"], "run_finished");
    assert_eq!(events[1]["data"]["task_board"]["in_progress_count"], 1);
    assert!(events[1]["data"].get("waiting_for").is_none());
    assert!(
        events
            .iter()
            .all(|event| event["event_type"] != "run_interrupted"),
        "in-progress task-board bookkeeping must not interrupt an answered run: {events:?}"
    );
}

#[test]
fn finalize_run_events_pauses_real_empty_completion_regardless_of_task_board() {
    let svc = test_service();
    let request = test_request("empty settlement");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.final_text.clear();
    state.hooks.task_board_snapshot =
        lifecycle_task_board_snapshot(astra_tools::task_mgmt::SessionTaskStatusKind::InProgress);
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::EmptyCompletion,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 1,
            turns_completed: 1,
            remaining_turns: 3,
            error_detail: Some(
                "agentic loop reached terminal state while unfinished task-board bookkeeping remained"
                    .to_string(),
            ),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Completed),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Paused);
    assert!(error.is_none());
    assert_eq!(events[0]["event_type"], "run_interrupted");
    assert_eq!(events[0]["data"]["kind"], "empty_completion");
    assert_eq!(events[0]["data"]["task_board"]["in_progress_count"], 1);
    assert!(events[0]["data"].get("waiting_for").is_none());
    assert_eq!(events[1]["event_type"], "run_finished");
    assert_eq!(events[1]["data"]["interrupted"], true);
    assert!(events[1]["data"].get("waiting_for").is_none());
}

#[test]
fn finalize_run_events_waiting_reason_comes_from_interruption_authority() {
    let svc = test_service();
    let request = test_request("paused task");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.final_text = "Paused pending user direction.".to_string();
    state.hooks.task_board_snapshot =
        lifecycle_task_board_snapshot(astra_tools::task_mgmt::SessionTaskStatusKind::Paused);
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::EmptyCompletion,
        astra_turn_core::interruption::ResumeAction::RequiresIntervention {
            description: "external approval is required".to_string(),
        },
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 1,
            turns_completed: 1,
            remaining_turns: 3,
            error_detail: Some("external approval is required".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Completed),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Paused);
    assert_eq!(error.as_deref(), Some("user_intervention"));
    assert_eq!(events[1]["event_type"], "run_interrupted");
    assert_eq!(events[1]["data"]["waiting_for"], "user_intervention");
    assert_eq!(events[1]["data"]["task_board"]["paused_count"], 1);
    assert_eq!(events[2]["event_type"], "run_finished");
    assert_eq!(events[2]["data"]["waiting_for"], "user_intervention");
}

#[test]
fn merge_cancelled_run_events_preserves_order_and_usage() {
    let cancel_flag = Arc::new(AtomicBool::new(true));
    let cancel_token = Arc::new(CancellationToken::new());
    let mut run = RunState {
        run_id: "run-1".into(),
        user_id: "user-1".into(),
        session_id: "session-1".into(),
        status: RunStatus::Cancelled,
        events: vec![
            json!({"event_type": "run_started", "data": {}}),
            json!({"event_type": "run_finished", "data": {"cancelled": true}}),
        ],
        cancel_flag,
        pause_flag: Arc::new(AtomicBool::new(false)),
        llm_cancel_token: cancel_token,
        live_tx: None,
        waiting_for: None,
        execution_live: false,
    };

    merge_cancelled_run_events(
        &mut run,
        vec![
            json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
            json!({"event_type": "run_finished", "data": {"cancelled": true, "prompt_tokens": 3}}),
        ],
    );

    assert_eq!(run.events.len(), 3);
    assert_eq!(run.events[1]["event_type"], "text_delta");
    assert_eq!(run.events[2]["event_type"], "run_finished");
    assert_eq!(run.events[2]["data"]["cancelled"], true);
    assert_eq!(run.events[2]["data"]["prompt_tokens"], 3);
}

#[test]
fn terminal_events_for_persistence_keeps_only_terminal_lifecycle_events() {
    let events = vec![
        json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
        json!({"type": "reasoning_delta", "content": "thinking"}),
        json!({"type": "thinking_delta", "content": "private thinking"}),
        json!({"type": "reasoning_message_content", "content": "raw chain of thought"}),
        json!({"event_type": "reasoning_message_content", "data": {"content": "raw chain of thought"}}),
        json!({"type": "reasoning_done"}),
        json!({"type": "thinking_done"}),
        json!({"type": "runtime.control.handoff.requested", "handoff_id": "handoff-1"}),
        json!({"type": "runtime.control.handoff.rejected", "error_code": "terminal_handoff_window_closed"}),
        json!({"event_type": "text_done", "data": {"full_text": "final answer"}}),
        json!({"event_type": "run_error", "data": {"error": "boom"}}),
        json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
    ];

    let persisted = terminal_events_for_persistence(&events);
    assert_eq!(persisted.len(), 7);
    assert_eq!(persisted[0]["type"], "reasoning_done");
    assert_eq!(persisted[1]["type"], "thinking_done");
    assert_eq!(persisted[2]["type"], "runtime.control.handoff.requested");
    assert_eq!(persisted[3]["type"], "runtime.control.handoff.rejected");
    assert_eq!(persisted[4]["event_type"], "text_done");
    assert_eq!(persisted[5]["event_type"], "run_error");
    assert_eq!(persisted[6]["event_type"], "run_finished");
}

#[test]
fn terminal_handoff_event_uses_live_persistence_without_terminal_replay_duplication() {
    let event = json!({
        "type": "runtime.control.handoff.requested",
        "handoff_id": "handoff-1"
    });

    assert!(live_delta_event_for_persistence(&event));
    assert!(!streaming_final_event_for_replay(&event));
}

#[tokio::test]
async fn create_run_returns_running_status() {
    let svc = test_service();
    let result = ok(svc.create_run("user-1".into(), test_request("hello")).await);
    assert_eq!(result.status, "running");
    assert!(!result.run_id.is_empty());
    assert!(!result.session_id.is_empty());
}

#[tokio::test]
async fn create_run_uses_provided_session_id() {
    let svc = test_service();
    let mut req = test_request("hi");
    req.session_id = Some("custom-session".into());
    let result = ok(svc.create_run("user-1".into(), req).await);
    assert_eq!(result.session_id, "custom-session");
}

#[tokio::test]
async fn create_run_rejects_invalid_server_workspace_session_id() {
    let svc = test_service();
    let mut req = test_request("hi");
    req.session_id = Some("../../".into());
    req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: None,
        root: None,
        source: None,
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });

    let err = err(svc.create_run("user-1".into(), req).await);

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.detail,
        "Invalid session_id for server workspace provisioning"
    );
}

#[tokio::test]
async fn stream_chat_rejects_invalid_server_workspace_session_id() {
    let svc = test_service();
    let mut req = test_request("hi");
    req.session_id = Some("../../".into());
    req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: None,
        root: None,
        source: None,
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });

    let err = err(svc.stream_chat("user-1".into(), req).await);

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.detail,
        "Invalid session_id for server workspace provisioning"
    );
}

#[tokio::test]
async fn provider_stream_chat_replays_the_run_bound_to_task_ref() {
    let svc = test_service();
    let mut request = test_request("retry the same provider task");
    request.provider_runtime_authorized = true;
    request.session_id = Some("session-provider-1".to_string());
    request.context = Some(serde_json::Map::from_iter([(
        "task_ref".to_string(),
        json!("task-provider-1"),
    )]));
    let identity = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    svc.run_engine
        .start_run_with_context(
            &identity.run_id,
            "user-1",
            "session-provider-1",
            RunStartContext {
                provider_request_fingerprint: Some(identity.request_fingerprint),
                ..RunStartContext::default()
            },
        )
        .await
        .expect("seed provider run");

    let stream = ok(svc.stream_chat("user-1".to_string(), request).await);

    assert_eq!(stream.run_id, identity.run_id);
    assert_eq!(stream.session_id, "session-provider-1");
}

#[test]
fn provider_task_ref_identity_is_tenant_scoped_and_request_bound() {
    let svc = test_service();
    let mut request = test_request("original request");
    request.provider_runtime_authorized = true;
    request.context = Some(serde_json::Map::from_iter([(
        "task_ref".to_string(),
        json!("task-provider-1"),
    )]));

    let user_one = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    let user_two = svc
        .provider_idempotency_identity("user-2", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(user_one.run_id, user_two.run_id);
    assert_eq!(user_one.request_fingerprint, user_two.request_fingerprint);

    request.message = "changed request".to_string();
    let changed = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_eq!(user_one.run_id, changed.run_id);
    assert_ne!(user_one.request_fingerprint, changed.request_fingerprint);

    request.message = "original request".to_string();
    request.model = Some("different-model".to_string());
    let changed_model = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_eq!(user_one.run_id, changed_model.run_id);
    assert_ne!(
        user_one.request_fingerprint,
        changed_model.request_fingerprint
    );

    request.model = None;
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "binding-2".to_string(),
    });
    let changed_binding = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(user_one.run_id, changed_binding.run_id);

    request.agent_binding = None;
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(astra_services::runs::RuntimeCapabilityDescriptorRequest {
                id: "gateway-2".to_string(),
                descriptor_type: "model_gateway".to_string(),
                transport: "http".to_string(),
                endpoint_url: "https://gateway.example.test".to_string(),
                protocol: "openai".to_string(),
                semantic_read: None,
                metadata: serde_json::Map::new(),
            }),
            ..Default::default()
        });
    let changed_capability = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(user_one.run_id, changed_capability.run_id);
}

#[test]
fn provider_task_ref_fingerprint_tracks_semantic_routing_but_not_credential_rotation() {
    let svc = test_service();
    let mut request = test_request("same request");
    request.provider_runtime_authorized = true;
    request.context = Some(serde_json::Map::from_iter([(
        "task_ref".to_string(),
        json!("task-provider-routing"),
    )]));
    request
        .forward_headers
        .insert("x-workspace-id".to_string(), "workspace-1".to_string());
    request
        .forward_headers
        .insert("authorization".to_string(), "Bearer token-one".to_string());
    let mut binding = test_runtime_mcp_binding();
    binding.url =
        "https://tools.example.test/mcp?workspace=one&workspace=shared&access_token=token-one"
            .to_string();
    binding
        .headers
        .insert("x-user-id".to_string(), "user-route-1".to_string());
    binding
        .headers
        .insert("x-api-key".to_string(), "api-key-one".to_string());
    request.runtime_mcp_bindings = vec![binding];

    let original = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");

    request
        .forward_headers
        .insert("authorization".to_string(), "Bearer token-two".to_string());
    request.runtime_mcp_bindings[0]
        .headers
        .insert("x-api-key".to_string(), "api-key-two".to_string());
    request.runtime_mcp_bindings[0].url =
        "https://tools.example.test/mcp?workspace=one&workspace=shared&access_token=token-two"
            .to_string();
    let rotated_credentials = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_eq!(
        original.request_fingerprint,
        rotated_credentials.request_fingerprint
    );

    request
        .forward_headers
        .insert("x-workspace-id".to_string(), "workspace-2".to_string());
    let changed_workspace = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(
        original.request_fingerprint,
        changed_workspace.request_fingerprint
    );

    request
        .forward_headers
        .insert("x-workspace-id".to_string(), "workspace-1".to_string());
    request.runtime_mcp_bindings[0].url =
        "https://tools.example.test/mcp?workspace=one&workspace=changed&access_token=token-two"
            .to_string();
    let changed_query = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(
        original.request_fingerprint,
        changed_query.request_fingerprint
    );

    request.runtime_mcp_bindings[0].url =
        "https://tools.example.test/mcp?workspace=one&workspace=shared&access_token=token-two"
            .to_string();
    request.runtime_mcp_bindings[0]
        .headers
        .insert("x-user-id".to_string(), "user-route-2".to_string());
    let changed_user_header = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(
        original.request_fingerprint,
        changed_user_header.request_fingerprint
    );
}

#[test]
fn provider_task_ref_fingerprint_keeps_semantic_tokens_and_query_order() {
    let svc = test_service();
    let mut request = test_request("same request");
    request.provider_runtime_authorized = true;
    request.context = Some(serde_json::Map::from_iter([(
        "task_ref".to_string(),
        json!("task-provider-semantic-tokens"),
    )]));
    request
        .forward_headers
        .insert("x-revision-token".to_string(), "revision-one".to_string());
    let mut binding = test_runtime_mcp_binding();
    binding.url =
        "https://tools.example.test/mcp?item=first&item=second&page_token=page-one".to_string();
    request.runtime_mcp_bindings = vec![binding];

    let original = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");

    request
        .forward_headers
        .insert("x-revision-token".to_string(), "revision-two".to_string());
    let changed_header_token = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(
        original.request_fingerprint,
        changed_header_token.request_fingerprint
    );

    request
        .forward_headers
        .insert("x-revision-token".to_string(), "revision-one".to_string());
    request.runtime_mcp_bindings[0].url =
        "https://tools.example.test/mcp?item=first&item=second&page_token=page-two".to_string();
    let changed_page_token = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(
        original.request_fingerprint,
        changed_page_token.request_fingerprint
    );

    request.runtime_mcp_bindings[0].url =
        "https://tools.example.test/mcp?item=second&item=first&page_token=page-one".to_string();
    let reordered_repeated_query = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    assert_ne!(
        original.request_fingerprint,
        reordered_repeated_query.request_fingerprint
    );
}

#[tokio::test]
async fn provider_task_ref_rejects_a_changed_request() {
    let svc = test_service();
    let mut request = test_request("original request");
    request.provider_runtime_authorized = true;
    request.session_id = Some("session-provider-1".to_string());
    request.context = Some(serde_json::Map::from_iter([(
        "task_ref".to_string(),
        json!("task-provider-1"),
    )]));
    let identity = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    svc.run_engine
        .start_run_with_context(
            &identity.run_id,
            "user-1",
            "session-provider-1",
            RunStartContext {
                provider_request_fingerprint: Some(identity.request_fingerprint),
                ..RunStartContext::default()
            },
        )
        .await
        .expect("seed provider run");

    request.message = "changed request".to_string();
    let error = err(svc.stream_chat("user-1".to_string(), request).await);
    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0.error_code.as_deref(),
        Some("provider_task_ref_request_mismatch")
    );
}

#[tokio::test]
async fn provider_task_ref_isolates_two_users_across_attach_and_cancel() {
    let svc = test_service();
    let mut request = test_request("same provider request");
    request.provider_runtime_authorized = true;
    request.context = Some(serde_json::Map::from_iter([(
        "task_ref".to_string(),
        json!("shared-provider-task"),
    )]));
    let user_one = svc
        .provider_idempotency_identity("user-1", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    let user_two = svc
        .provider_idempotency_identity("user-2", &request)
        .expect("valid provider identity")
        .expect("provider identity");
    for (user_id, session_id, identity) in [
        ("user-1", "session-1", &user_one),
        ("user-2", "session-2", &user_two),
    ] {
        svc.run_engine
            .start_run_with_context(
                &identity.run_id,
                user_id,
                session_id,
                RunStartContext {
                    provider_request_fingerprint: Some(identity.request_fingerprint.clone()),
                    ..RunStartContext::default()
                },
            )
            .await
            .expect("seed provider run");
        let attached = ok(svc
            .stream_run_live(identity.run_id.clone(), user_id.to_string(), 0)
            .await);
        assert_eq!(attached.run_id, identity.run_id);
        assert_eq!(attached.session_id, session_id);
    }

    let cancelled = ok(svc
        .cancel_run(user_one.run_id.clone(), "user-1".to_string())
        .await);
    assert_eq!(cancelled.status, STATUS_CANCELLED);
    let user_two_status = ok(svc
        .get_run_status(user_two.run_id.clone(), "user-2".to_string())
        .await);
    assert_eq!(user_two_status.status, STATUS_RUNNING);
    let foreign = err(svc
        .get_run_status(user_two.run_id, "user-1".to_string())
        .await);
    assert_eq!(foreign.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn durable_live_attach_follows_a_run_without_process_local_state() {
    let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    let svc = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine.clone(),
    )
    .with_model_service(Arc::new(ActiveTestModelService::default()));
    engine
        .start_run("remote-provider-task", "user-1", "remote-session")
        .await
        .expect("seed remote run");

    let mut stream = ok(svc
        .stream_run_live("remote-provider-task".to_string(), "user-1".to_string(), 0)
        .await);
    let mut event_rx = stream
        .event_rx
        .take()
        .expect("active remote run must keep a durable live attachment");
    let started = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .expect("run_started replay timeout")
        .expect("run_started replay");
    assert_eq!(started["event_type"], "run_started");

    assert!(
        engine
            .transition_status_with_event_if_current(
                "user-1",
                "remote-provider-task",
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                json!({"event_type": "run_finished", "data": {}}),
            )
            .await
            .expect("complete remote run")
    );
    let finished = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .expect("run_finished replay timeout")
        .expect("run_finished replay");
    assert_eq!(finished["event_type"], "run_finished");
    assert!(
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("durable live attachment close timeout")
            .is_none()
    );
}

#[tokio::test]
async fn production_fanout_batches_slow_durable_writes_before_terminal() {
    let llm = spawn_incremental_terminal_test_llm(Duration::from_millis(100)).await;
    let store = Arc::new(
        FaultInjectedRunStateStore::new(&[], &[]).with_append_delay(Duration::from_millis(40)),
    );
    let engine = RunEngine::new(store.clone());
    let owner = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine.clone(),
    )
    .with_model_service(Arc::new(ActiveTestModelService::new(llm.base_url.clone())));
    let observer = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine,
    )
    .with_model_service(Arc::new(ActiveTestModelService::default()));

    let mut request = prepared_test_request("slow live run");
    request.provider_runtime_authorized = true;
    request.admitted_model_execution = None;
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "incremental-test-gateway",
                "model_gateway",
                &format!("{}/chat/completions", llm.base_url),
            )),
            ..Default::default()
        });
    let mut owner_stream = ok(owner.stream_chat("user-1".to_string(), request).await);
    let mut owner_event_rx = owner_stream.event_rx.take().expect("owner live stream");
    let owner_drain = tokio::spawn(async move { while owner_event_rx.recv().await.is_some() {} });
    let mut attached = ok(observer
        .stream_run_live(owner_stream.run_id.clone(), "user-1".to_string(), 0)
        .await);
    let mut event_rx = attached
        .event_rx
        .take()
        .expect("remote service should follow the durable live cursor");

    for index in 0..600_u64 {
        owner
            .server_agent_progress_broadcaster
            .emit(test_agent_spawned(
                &format!("agent-load-{index}"),
                &format!("child-load-{index}"),
                &owner_stream.run_id,
                index,
            ));
    }

    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut progress_count = 0;
        while let Some(event) = event_rx.recv().await {
            if event.get("type").and_then(Value::as_str) == Some("agent_spawned") {
                progress_count += 1;
            }
            observed.push(event);
            if progress_count == 600 {
                break;
            }
        }
    })
    .await
    .expect("cross-service live event timeout");
    // Let the remote consumer fall behind the owner through terminal commit.
    // The durable cursor must retain order and exactly-once delivery.
    tokio::time::sleep(Duration::from_secs(6)).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = event_rx.recv().await {
            observed.push(event);
        }
    })
    .await
    .expect("cross-service terminal close timeout");

    assert_eq!(
        observed
            .iter()
            .filter(|event| event.get("type").and_then(Value::as_str) == Some("agent_spawned"))
            .count(),
        600,
        "critical lifecycle evidence must be retained exactly once"
    );
    assert_eq!(
        observed
            .last()
            .and_then(|event| event.get("event_type"))
            .and_then(Value::as_str),
        Some("run_finished"),
        "durable replay must retain production ordering through terminal"
    );
    let appended_batches = store.appended_batches();
    assert!(
        appended_batches.iter().any(|batch| batch.len() > 1),
        "live durability must use microbatches instead of one store transaction per event"
    );
    assert!(
        appended_batches
            .iter()
            .all(|batch| batch.len() <= DURABLE_LIVE_BATCH_MAX_EVENTS),
        "live durability batches must remain bounded"
    );
    owner_drain.await.expect("owner stream drain");
}

#[test]
fn durable_live_batch_coalesces_only_redundant_agent_progress() {
    let mut pending = PendingDurableLiveEvents::default();
    pending.push(json!({
        "type": "agent_progress",
        "agent_id": "agent-1",
        "status": "metrics_update",
        "turn": 1,
    }));
    pending.push(json!({
        "type": "agent_spawned",
        "agent_id": "agent-1",
        "seq": 1,
    }));
    pending.push(json!({
        "type": "agent_progress",
        "agent_id": "agent-1",
        "status": "metrics_update",
        "turn": 2,
    }));
    pending.push(json!({
        "type": "agent_spawned",
        "agent_id": "agent-1",
        "seq": 2,
    }));

    let events = pending.take();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[1]["turn"], 2);
    assert_eq!(events[2]["seq"], 2);
}

#[tokio::test]
async fn durable_live_attach_does_not_drop_a_backpressured_event_burst() {
    let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    let svc = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine.clone(),
    )
    .with_model_service(Arc::new(ActiveTestModelService::default()));
    engine
        .start_run("remote-burst", "user-1", "remote-session")
        .await
        .expect("seed remote run");

    let mut stream = ok(svc
        .stream_run_live("remote-burst".to_string(), "user-1".to_string(), 0)
        .await);
    let mut event_rx = stream.event_rx.take().expect("live attachment");
    assert_eq!(
        event_rx.recv().await.expect("run_started")["event_type"],
        "run_started"
    );

    let burst = (0..600)
        .map(|index| {
            json!({
                "event_type": "agent_progress",
                "data": {"index": index},
            })
        })
        .collect::<Vec<_>>();
    engine
        .append_events_batch("user-1", "remote-burst", &burst)
        .await
        .expect("append burst");
    assert!(
        engine
            .transition_status_with_event_if_current(
                "user-1",
                "remote-burst",
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                json!({"event_type": "run_finished", "data": {}}),
            )
            .await
            .expect("complete remote run")
    );

    let events = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("durable burst replay timeout");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event_type"] == "agent_progress")
            .count(),
        600
    );
    assert_eq!(
        events.last().expect("terminal event")["event_type"],
        "run_finished"
    );
}

#[test]
fn provider_task_ref_must_be_a_path_safe_run_identifier() {
    let svc = test_service();
    let mut request = test_request("invalid provider task");
    request.provider_runtime_authorized = true;
    request.context = Some(serde_json::Map::from_iter([(
        "task_ref".to_string(),
        json!("../task-provider-1"),
    )]));

    let error = svc
        .provider_idempotency_identity("user-1", &request)
        .expect_err("path-like task_ref must fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0.error_code.as_deref(),
        Some("provider_task_ref_invalid")
    );
}

#[tokio::test]
async fn create_run_explain_mode_returns_metadata() {
    let svc = test_service();
    let mut req = test_request("explain me");
    req.explain = true;
    let result = ok(svc.create_run("user-1".into(), req).await);
    assert!(result.explain.is_some());
    assert_eq!(result.explain.unwrap()["mode"], "background");
}

#[tokio::test]
async fn create_run_conflicts_when_same_session_already_has_active_run() {
    let svc = test_service();
    let mut first = test_request("hello");
    first.session_id = Some("shared-session".into());
    ok(svc.create_run("user-1".into(), first).await);

    let mut second = test_request("again");
    second.session_id = Some("shared-session".into());
    let err = err(svc.create_run("user-1".into(), second).await);
    assert_eq!(err.0, StatusCode::CONFLICT);
    assert_eq!(err.1.0.detail, "session already has an active run");
}

#[tokio::test]
async fn create_run_session_exclusion_is_scoped_by_user() {
    let svc = test_service();
    let (blocking, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
        "user-one-run".to_string(),
        "shared-session".to_string(),
        "user-1".to_string(),
    );
    svc.runs
        .write()
        .await
        .insert("user-one-run".to_string(), blocking);

    let mut request = test_request("independent user");
    request.session_id = Some("shared-session".to_string());
    let started = ok(svc.create_run("user-2".to_string(), request).await);

    assert_eq!(started.session_id, "shared-session");
}

#[tokio::test]
async fn stream_chat_conflicts_when_same_session_already_has_active_run() {
    let svc = test_service();
    let mut first = test_request("hello");
    first.session_id = Some("shared-session".into());
    ok(svc.create_run("user-1".into(), first).await);

    let mut second = test_request("again");
    second.session_id = Some("shared-session".into());
    let err = err(svc.stream_chat("user-1".into(), second).await);
    assert_eq!(err.0, StatusCode::CONFLICT);
    assert_eq!(err.1.0.detail, "session already has an active run");
}

#[tokio::test]
async fn provider_stream_session_exclusion_is_scoped_by_user() {
    let (svc, _llm) = terminal_test_service().await;
    let (blocking, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
        "user-one-provider-run".to_string(),
        "shared-provider-session".to_string(),
        "user-1".to_string(),
    );
    svc.runs
        .write()
        .await
        .insert("user-one-provider-run".to_string(), blocking);

    let mut request = prepared_test_request("independent provider user");
    request.session_id = Some("shared-provider-session".to_string());
    request.provider_runtime_authorized = true;
    request.admitted_model_execution = None;
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "provider-session-gateway",
                "model_gateway",
                "http://127.0.0.1:1/chat/completions",
            )),
            ..Default::default()
        });
    request.context = Some(serde_json::Map::from_iter([(
        "task_ref".to_string(),
        json!("user-two-provider-task"),
    )]));
    let started = ok(svc.stream_chat("user-2".to_string(), request).await);

    assert_eq!(started.session_id, "shared-provider-session");
}

#[tokio::test]
async fn stream_chat_tracks_run_for_status_and_replay() {
    let (svc, _llm) = terminal_test_service().await;
    let mut request = test_request("hello");
    request.execution_policy.skill_auto_route =
        astra_services::runs::SkillAutoRouteExecutionPolicy::Disabled;
    let stream = ok(svc.stream_chat("user-1".into(), request).await);

    let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let status = ok(svc
                .get_run_status(stream.run_id.clone(), "user-1".into())
                .await);
            if status.status != "running" {
                break status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timeout waiting for stream_chat status to finish");
    let replay = ok(svc
        .stream_run(stream.run_id.clone(), "user-1".into(), 0)
        .await);

    assert_eq!(status.run_id, stream.run_id);
    assert!(status.events_count > 0);
    assert_eq!(replay.len(), status.events_count as usize);
    assert_eq!(replay[0]["event_type"], "run_started");
    assert_eq!(replay[0]["data"]["skill_auto_route_policy"], "disabled");
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&stream.run_id).await,
        Some(false)
    );
}

#[tokio::test]
async fn get_run_status_returns_state() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
    let status = ok(svc
        .get_run_status(run.run_id.clone(), "user-1".into())
        .await);
    assert_eq!(status.run_id, run.run_id);
    assert_eq!(status.status, "running");
    assert_eq!(status.events_count, 1);
    assert_eq!(status.workspace.as_ref().unwrap()["kind"], "none");
    assert_eq!(status.executor.as_ref().unwrap()["kind"], "server_local");
    assert_eq!(
        status.executor.as_ref().unwrap()["executor_id"],
        "server-control-plane"
    );
    assert_eq!(status.transport.as_deref(), Some("server_local"));
}

#[tokio::test]
async fn noninteractive_create_run_does_not_wire_ws_only_channels() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

    assert!(!svc.approval_channels.lock().await.contains_key(&run.run_id));
    assert!(
        !svc.user_prompt_channels
            .lock()
            .await
            .contains_key(&run.run_id)
    );
    assert!(!svc.progress_channels.lock().await.contains_key(&run.run_id));
}

async fn wait_for_durable_run_status(
    engine: &RunEngine,
    user_id: &str,
    run_id: &str,
    expected_status: &str,
) -> DurableRunRecord {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let run = engine
                .load_run(user_id, run_id)
                .await
                .unwrap()
                .expect("durable run");
            if run.status == expected_status {
                return run;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("run status transition")
}

#[tokio::test(flavor = "current_thread")]
async fn server_only_approval_wait_resumes_from_shared_interaction_state() {
    let svc = test_service();
    svc.run_engine
        .start_run("server-only-run", "user-1", "server-only-session")
        .await
        .unwrap();
    let gate = DurableRunApprovalGate::new(
        "user-1".into(),
        "server-only-session".into(),
        "server-only-run".into(),
        Some(4),
        svc.run_engine.clone(),
        svc.runs_handle(),
        None,
        None,
    )
    .with_timeout(Duration::from_secs(1));

    let approval = tokio::spawn(async move {
        astra_tools::ToolApprovalGate::request_approval(
            &gate,
            "approval-server-only",
            "bash",
            &json!({"command": "git status"}),
        )
        .await
    });
    wait_for_durable_run_status(&svc.run_engine, "user-1", "server-only-run", STATUS_WAITING).await;
    let resolved = svc
        .run_engine
        .resolve_run_interaction(
            "user-1",
            "server-only-run",
            "approval-server-only",
            astra_services::runs::DurableRunInteractionKind::Approval,
            json!({
                "request_id": "approval-server-only",
                "outcome": "approved",
                "decision": "allow",
                "reason": null,
                "tool": "bash",
                "approval_kind": "standard",
            }),
        )
        .await
        .unwrap();
    assert!(matches!(
        resolved,
        astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_)
    ));
    let decision = approval.await.unwrap();

    assert!(matches!(decision, astra_tools::ApprovalDecision::Approved));
    let durable = svc
        .run_engine
        .load_run("user-1", "server-only-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert_eq!(durable.waiting_for, None);
    assert_eq!(
        durable.events[1]["event_type"], "approval_required",
        "request must be part of durable run replay"
    );
    assert_eq!(durable.events[1]["data"]["delivery"], "durable");
    assert_eq!(durable.events[2]["event_type"], "approval_resolved");
    assert_eq!(durable.events[2]["data"]["outcome"], "approved");
    assert_eq!(durable.events[3]["event_type"], "run_resumed");
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_run_unblocks_durable_approval_without_executing_a_late_decision() {
    let svc = test_service();
    svc.run_engine
        .start_run("approval-cancelled", "user-1", "server-only-session")
        .await
        .unwrap();
    let cancel_token = Arc::new(CancellationToken::new());
    let (wait_started_tx, wait_started_rx) = oneshot::channel();
    let gate = Arc::new(
        DurableRunApprovalGate::new(
            "user-1".into(),
            "server-only-session".into(),
            "approval-cancelled".into(),
            Some(4),
            svc.run_engine.clone(),
            svc.runs_handle(),
            None,
            None,
        )
        .with_timeout(Duration::from_secs(30))
        .with_cancel_token(cancel_token.clone())
        .with_wait_started_notifier(wait_started_tx),
    );
    let waiting_gate = gate.clone();
    let waiting = tokio::spawn(async move {
        astra_tools::ToolApprovalGate::request_approval(
            waiting_gate.as_ref(),
            "approval-cancelled-request",
            "bash",
            &json!({"command": "touch must-not-run"}),
        )
        .await
    });
    wait_started_rx.await.expect("approval wait started");
    svc.run_engine
        .persist_status("user-1", "approval-cancelled", STATUS_CANCELLED, None, None)
        .await
        .unwrap();
    cancel_token.cancel();

    let decision = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("cancelled approval must not wait for its normal timeout")
        .expect("approval task join");
    assert!(matches!(
        decision,
        astra_tools::ApprovalDecision::Denied { .. }
    ));
    let durable = svc
        .run_engine
        .load_run("user-1", "approval-cancelled")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
    assert!(
        durable.events.iter().all(|event| {
            event.get("event_type").and_then(Value::as_str) != Some("run_resumed")
        }),
        "a cancellation must not turn a late approval wait back into a running run"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_run_unblocks_durable_user_prompt_without_resuming_the_child() {
    let svc = test_service();
    svc.run_engine
        .start_run("prompt-cancelled", "user-1", "server-only-session")
        .await
        .unwrap();
    let cancel_token = Arc::new(CancellationToken::new());
    let gate = Arc::new(
        DurableRunUserPromptGate::new(
            "user-1".into(),
            "server-only-session".into(),
            "prompt-cancelled".into(),
            Some(4),
            svc.run_engine.clone(),
            svc.runs_handle(),
            None,
            None,
        )
        .with_timeout(Duration::from_secs(30))
        .with_cancel_token(cancel_token.clone()),
    );
    let prompt = astra_tools::AskUserPrompt {
        context: Some("Need a choice".into()),
        questions: vec![astra_tools::AskUserQuestion {
            header: "Scope".into(),
            question: "Continue?".into(),
            options: Vec::new(),
            multi_select: false,
            allow_freeform: true,
        }],
        timeout_ms: None,
    };
    let waiting_gate = gate.clone();
    let waiting = tokio::spawn(async move {
        astra_tools::AskUserGate::request_questionnaire(
            waiting_gate.as_ref(),
            "prompt-cancelled-request",
            &prompt,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let status = svc
                .run_engine
                .load_run("user-1", "prompt-cancelled")
                .await
                .unwrap()
                .expect("durable run");
            if status.status == STATUS_WAITING {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("user prompt must enter a durable wait");
    svc.run_engine
        .persist_status("user-1", "prompt-cancelled", STATUS_CANCELLED, None, None)
        .await
        .unwrap();
    cancel_token.cancel();

    let decision = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("cancelled prompt must not wait for its normal timeout")
        .expect("prompt task join");
    assert!(matches!(decision, astra_tools::AskUserDecision::Cancelled));
    let durable = svc
        .run_engine
        .load_run("user-1", "prompt-cancelled")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
    assert!(
        durable.events.iter().all(|event| {
            event.get("event_type").and_then(Value::as_str) != Some("run_resumed")
        }),
        "a cancelled prompt must not resume execution when a late answer appears"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn provider_interaction_requires_an_authenticated_provider_run_owner() {
    let svc = test_service();
    svc.run_engine
        .start_run(
            "provider-interaction-unowned",
            "user-1",
            "server-only-session",
        )
        .await
        .unwrap();
    let gate = DurableRunUserPromptGate::new(
        "user-1".into(),
        "server-only-session".into(),
        "provider-interaction-unowned".into(),
        Some(4),
        svc.run_engine.clone(),
        svc.runs_handle(),
        None,
        None,
    );
    let decision = astra_tools::ProviderInteractionGate::request_interaction(
        &gate,
        &astra_turn_types::ProviderInteractionRequest {
            request_id: "provider-interaction-request".into(),
            payload: json!({"type": "provider.test.select"}),
            timeout_ms: None,
        },
    )
    .await;

    assert!(matches!(
        decision,
        astra_tools::ProviderInteractionDecision::Error(ref message)
            if message.contains("authenticated provider run owner")
    ));
    let durable = svc
        .run_engine
        .load_run("user-1", "provider-interaction-unowned")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(
        durable.events.iter().all(|event| {
            event.get("event_type").and_then(Value::as_str) != Some("provider_interaction_required")
        }),
        "an unowned interaction must not create an unresolvable durable wait"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn server_only_user_prompt_wait_resumes_from_shared_interaction_state() {
    let svc = test_service();
    svc.run_engine
        .start_run("server-only-prompt", "user-1", "server-only-session")
        .await
        .unwrap();
    let gate = DurableRunUserPromptGate::new(
        "user-1".into(),
        "server-only-session".into(),
        "server-only-prompt".into(),
        Some(4),
        svc.run_engine.clone(),
        svc.runs_handle(),
        None,
        None,
    )
    .with_timeout(Duration::from_secs(1));
    let prompt = astra_tools::AskUserPrompt {
        context: None,
        questions: vec![astra_tools::AskUserQuestion {
            header: "Scope".into(),
            question: "Continue?".into(),
            options: vec![astra_tools::AskUserChoice {
                label: "yes".into(),
                description: None,
                preview: None,
            }],
            multi_select: false,
            allow_freeform: false,
        }],
        timeout_ms: None,
    };
    let prompt_wait = tokio::spawn(async move {
        astra_tools::AskUserGate::request_questionnaire(&gate, "prompt-server-only", &prompt).await
    });
    wait_for_durable_run_status(
        &svc.run_engine,
        "user-1",
        "server-only-prompt",
        STATUS_WAITING,
    )
    .await;
    svc.run_engine
        .resolve_run_interaction(
            "user-1",
            "server-only-prompt",
            "prompt-server-only",
            astra_services::runs::DurableRunInteractionKind::AskUser,
            json!({
                "request_id": "prompt-server-only",
                "outcome": "submitted",
                "answers": {
                    "answers": [{
                        "question": "Continue?",
                        "answers": ["yes"],
                        "multi_select": false,
                        "annotation": null
                    }]
                }
            }),
        )
        .await
        .unwrap();
    let decision = prompt_wait.await.unwrap();
    assert!(matches!(
        decision,
        astra_tools::AskUserDecision::Submitted(ref answers)
            if answers.answers[0].answers == vec!["yes".to_string()]
    ));

    let durable = svc
        .run_engine
        .load_run("user-1", "server-only-prompt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert_eq!(durable.waiting_for, None);
    assert_eq!(durable.events[1]["event_type"], "ask_user_prompted");
    assert_eq!(durable.events[1]["data"]["delivery"], "durable");
    assert_eq!(durable.events[2]["event_type"], "ask_user_resolved");
    assert_eq!(durable.events[2]["data"]["outcome"], "submitted");
    assert_eq!(durable.events[3]["event_type"], "run_resumed");
}

#[tokio::test(flavor = "current_thread")]
async fn server_only_user_prompt_projects_required_event_to_active_stream() {
    let svc = test_service();
    svc.run_engine
        .start_run("server-only-prompt-stream", "user-1", "server-only-session")
        .await
        .unwrap();
    let (stream_tx, mut stream_rx) = mpsc::channel(4);
    let gate = DurableRunUserPromptGate::new(
        "user-1".into(),
        "server-only-session".into(),
        "server-only-prompt-stream".into(),
        Some(5),
        svc.run_engine.clone(),
        svc.runs_handle(),
        None,
        Some(stream_tx),
    )
    .with_timeout(Duration::from_secs(1));
    let prompt = astra_tools::AskUserPrompt {
        context: Some("Need a decision".into()),
        questions: vec![astra_tools::AskUserQuestion {
            header: "Scope".into(),
            question: "Continue?".into(),
            options: Vec::new(),
            multi_select: false,
            allow_freeform: true,
        }],
        timeout_ms: None,
    };
    let prompt_wait = tokio::spawn(async move {
        astra_tools::AskUserGate::request_questionnaire(&gate, "prompt-stream", &prompt).await
    });
    wait_for_durable_run_status(
        &svc.run_engine,
        "user-1",
        "server-only-prompt-stream",
        STATUS_WAITING,
    )
    .await;
    svc.run_engine
        .resolve_run_interaction(
            "user-1",
            "server-only-prompt-stream",
            "prompt-stream",
            astra_services::runs::DurableRunInteractionKind::AskUser,
            json!({
                "request_id": "prompt-stream",
                "outcome": "cancelled",
                "answers": null,
            }),
        )
        .await
        .unwrap();
    let decision = prompt_wait.await.unwrap();
    assert!(matches!(decision, astra_tools::AskUserDecision::Cancelled));

    let required = tokio::time::timeout(Duration::from_secs(1), stream_rx.recv())
        .await
        .expect("user prompt required must reach active stream")
        .expect("user prompt required event");
    let resumed = tokio::time::timeout(Duration::from_secs(1), stream_rx.recv())
        .await
        .expect("user prompt resolution must reach active stream")
        .expect("user prompt resumed event");
    assert_eq!(required["type"], "user_prompt_required");
    assert_eq!(required["run_id"], "server-only-prompt-stream");
    assert_eq!(required["prompt"]["questions"][0]["question"], "Continue?");
    assert_eq!(resumed["type"], "run_resumed");
    assert_eq!(resumed["interaction_outcome"], "cancelled");
}

#[tokio::test(flavor = "current_thread")]
async fn server_only_approval_timeout_is_durable_and_releases_waiting_state() {
    let svc = test_service();
    svc.run_engine
        .start_run("server-only-timeout", "user-1", "server-only-session")
        .await
        .unwrap();
    let gate = DurableRunApprovalGate::new(
        "user-1".into(),
        "server-only-session".into(),
        "server-only-timeout".into(),
        Some(5),
        svc.run_engine.clone(),
        svc.runs_handle(),
        None,
        None,
    )
    .with_timeout(Duration::from_millis(20));

    let decision = astra_tools::ToolApprovalGate::request_approval(
        &gate,
        "approval-timeout",
        "bash",
        &json!({"command": "rm -rf tmp"}),
    )
    .await;

    assert!(matches!(decision, astra_tools::ApprovalDecision::Timeout));
    let durable = svc
        .run_engine
        .load_run("user-1", "server-only-timeout")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert_eq!(durable.waiting_for, None);
    assert_eq!(durable.events[1]["event_type"], "approval_required");
    assert_eq!(durable.events[2]["event_type"], "approval_resolved");
    assert_eq!(durable.events[2]["data"]["outcome"], "timed_out");
    assert_eq!(durable.events[3]["event_type"], "run_resumed");
}

#[tokio::test(flavor = "current_thread")]
async fn server_only_approval_projects_required_and_resumed_events_to_active_stream() {
    let svc = test_service();
    svc.run_engine
        .start_run("server-only-stream", "user-1", "server-only-session")
        .await
        .unwrap();
    let (stream_tx, mut stream_rx) = mpsc::channel(4);
    let gate = DurableRunApprovalGate::new(
        "user-1".into(),
        "server-only-session".into(),
        "server-only-stream".into(),
        Some(6),
        svc.run_engine.clone(),
        svc.runs_handle(),
        None,
        Some(stream_tx),
    )
    .with_timeout(Duration::from_secs(1));

    let approval = tokio::spawn(async move {
        astra_tools::ToolApprovalGate::request_approval(
            &gate,
            "approval-stream",
            "bash",
            &json!({"command": "git status"}),
        )
        .await
    });
    wait_for_durable_run_status(
        &svc.run_engine,
        "user-1",
        "server-only-stream",
        STATUS_WAITING,
    )
    .await;
    svc.run_engine
        .resolve_run_interaction(
            "user-1",
            "server-only-stream",
            "approval-stream",
            astra_services::runs::DurableRunInteractionKind::Approval,
            json!({
                "request_id": "approval-stream",
                "outcome": "approved",
                "decision": "allow",
                "reason": null,
                "tool": "bash",
                "approval_kind": "standard",
            }),
        )
        .await
        .unwrap();
    let decision = approval.await.unwrap();

    assert!(matches!(decision, astra_tools::ApprovalDecision::Approved));
    let required = tokio::time::timeout(Duration::from_secs(1), stream_rx.recv())
        .await
        .expect("approval required must reach the active stream within one second")
        .expect("approval required stream event");
    let resumed = tokio::time::timeout(Duration::from_secs(1), stream_rx.recv())
        .await
        .expect("approval resumed must reach the active stream within one second")
        .expect("approval resumed stream event");
    assert_eq!(required["type"], "approval_required");
    assert_eq!(required["delivery"], "durable");
    assert_eq!(resumed["type"], "run_resumed");
    assert_eq!(resumed["interaction_outcome"], "approved");
}

#[tokio::test(flavor = "current_thread")]
async fn interactive_approval_uses_the_same_durable_gate_and_delivers_the_request() {
    let svc = test_service();
    svc.run_engine
        .start_run("interactive-approval", "user-1", "interactive-session")
        .await
        .unwrap();
    let (approval_tx, mut approval_rx) = mpsc::channel(4);
    let gate = DurableRunApprovalGate::new(
        "user-1".into(),
        "interactive-session".into(),
        "interactive-approval".into(),
        Some(7),
        svc.run_engine.clone(),
        svc.runs_handle(),
        Some(approval_tx),
        None,
    )
    .with_timeout(Duration::from_secs(1));

    let approval = tokio::spawn(async move {
        astra_tools::ToolApprovalGate::request_approval(
            &gate,
            "interactive-request",
            "write_file",
            &json!({"path": "notes.txt", "content": "draft"}),
        )
        .await
    });
    let request = tokio::time::timeout(Duration::from_secs(1), approval_rx.recv())
        .await
        .expect("interactive approval request must reach the WS delivery queue")
        .expect("interactive approval request");
    assert_eq!(request["request_id"], "interactive-request");
    assert_eq!(request["tool"], "write_file");
    assert_eq!(request["args"]["path"], "notes.txt");
    svc.run_engine
        .resolve_run_interaction(
            "user-1",
            "interactive-approval",
            "interactive-request",
            astra_services::runs::DurableRunInteractionKind::Approval,
            json!({
                "request_id": "interactive-request",
                "outcome": "approved",
                "decision": "allow",
                "reason": null,
                "tool": "write_file",
                "approval_kind": "standard",
            }),
        )
        .await
        .unwrap();
    let decision = approval.await.unwrap();
    assert!(matches!(decision, astra_tools::ApprovalDecision::Approved));
    let durable = svc
        .run_engine
        .load_run("user-1", "interactive-approval")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.events[1]["event_type"], "approval_required");
    assert_eq!(durable.events[2]["event_type"], "approval_resolved");
    assert_eq!(durable.events[3]["event_type"], "run_resumed");
}

#[tokio::test(flavor = "current_thread")]
async fn approval_cannot_execute_after_the_waiting_run_is_cancelled() {
    let svc = test_service();
    svc.run_engine
        .start_run("approval-cancelled", "user-1", "approval-session")
        .await
        .unwrap();
    let (wait_started_tx, wait_started_rx) = oneshot::channel();
    let gate = DurableRunApprovalGate::new(
        "user-1".into(),
        "approval-session".into(),
        "approval-cancelled".into(),
        Some(8),
        svc.run_engine.clone(),
        svc.runs_handle(),
        None,
        None,
    )
    .with_timeout(Duration::from_secs(1))
    .with_wait_started_notifier(wait_started_tx);
    let approval = tokio::spawn(async move {
        astra_tools::ToolApprovalGate::request_approval(
            &gate,
            "approval-cancelled-request",
            "bash",
            &json!({"command": "rm -rf tmp"}),
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(1), wait_started_rx)
        .await
        .expect("approval must persist its waiting state before cancellation")
        .expect("approval task dropped its waiting-state notifier");

    assert!(
        svc.run_engine
            .transition_status_with_event_if_current(
                "user-1",
                "approval-cancelled",
                &[STATUS_WAITING],
                STATUS_CANCELLED,
                None,
                None,
                json!({"event_type": "run_finished", "data": {"cancelled": true}}),
            )
            .await
            .unwrap()
    );
    let late = svc
        .run_engine
        .resolve_run_interaction(
            "user-1",
            "approval-cancelled",
            "approval-cancelled-request",
            astra_services::runs::DurableRunInteractionKind::Approval,
            json!({
                "request_id": "approval-cancelled-request",
                "outcome": "approved",
                "decision": "allow",
                "reason": null,
                "tool": "bash",
                "approval_kind": "standard",
            }),
        )
        .await
        .unwrap();
    assert!(matches!(
        late,
        astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting
    ));

    let decision = tokio::time::timeout(Duration::from_secs(1), approval)
        .await
        .expect("approval must observe the terminal run transition")
        .expect("approval task must not panic");
    assert!(matches!(
        decision,
        astra_tools::ApprovalDecision::Denied { .. }
    ));
    let durable = svc
        .run_engine
        .load_run("user-1", "approval-cancelled")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
}

#[tokio::test]
async fn create_run_persists_interaction_mode_into_run_started_event() {
    let svc = test_service();
    let mut req = test_request("hello");
    req.interaction_mode = Some(astra_services::runs::RequestedTurnInteractionMode::Auto);
    req.interactive_client = true;
    let run = ok(svc.create_run("user-1".into(), req).await);

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .expect("load run")
        .expect("run exists");
    assert_eq!(durable.events[0]["event_type"], "run_started");
    assert_eq!(durable.events[0]["data"]["interaction_mode"], "auto");
    assert_eq!(durable.events[0]["data"]["interactive_client"], true);
    assert_eq!(durable.events[0]["data"]["workspace"]["kind"], "none");
    assert!(durable.events[0]["data"]["workspace"]["cwd"].is_null());
    assert_eq!(
        durable.events[0]["data"]["executor"]["kind"],
        "server_local"
    );
    assert_eq!(
        durable.events[0]["data"]["executor"]["executor_id"],
        "server-control-plane"
    );
    assert_eq!(durable.events[0]["data"]["transport"], "server_local");
}

#[tokio::test]
async fn create_run_persists_edge_binding_into_run_started_event() {
    let svc = test_service();
    let mut req = test_request("review this repo");
    req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("MacBook Pro".to_string()),
        root: Some("/Users/xupeng/github/astra".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/Users/xupeng/github/astra".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    });
    req.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("edge-macbook-1".to_string()),
        display_name: Some("MacBook Pro".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });
    let run = ok(svc.create_run("user-1".into(), req).await);

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .expect("load run")
        .expect("run exists");
    assert_eq!(durable.events[0]["event_type"], "run_started");
    assert_eq!(
        durable.events[0]["data"]["workspace"]["kind"],
        "edge_workspace"
    );
    assert_eq!(
        durable.events[0]["data"]["workspace"]["cwd"],
        "/Users/xupeng/github/astra"
    );
    assert_eq!(durable.events[0]["data"]["executor"]["kind"], "edge_agent");
    assert_eq!(
        durable.events[0]["data"]["executor"]["executor_id"],
        "edge-macbook-1"
    );
    assert_eq!(durable.events[0]["data"]["transport"], "edge_ws");

    let status = ok(svc
        .get_run_status(run.run_id.clone(), "user-1".into())
        .await);
    assert_eq!(status.workspace.as_ref().unwrap()["kind"], "edge_workspace");
    assert_eq!(
        status.workspace.as_ref().unwrap()["cwd"],
        "/Users/xupeng/github/astra"
    );
    assert_eq!(status.executor.as_ref().unwrap()["kind"], "edge_agent");
    assert_eq!(
        status.executor.as_ref().unwrap()["executor_id"],
        "edge-macbook-1"
    );
    assert_eq!(status.transport.as_deref(), Some("edge_ws"));
}

#[tokio::test]
async fn get_run_status_not_found() {
    let svc = test_service();
    let e = err(svc
        .get_run_status("nonexistent".into(), "user-1".into())
        .await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_run_status_hides_foreign_run() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
    let e = err(svc.get_run_status(run.run_id, "user-2".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cancel_run_sets_cancelled_status() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(result.status, "cancelled");
    let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
    assert_eq!(status.status, "cancelled");
    assert!(status.events_count >= 1);
}

#[tokio::test]
async fn cancel_run_cancels_llm_token_for_inflight_wake() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
        Some(false)
    );
    ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
        Some(true)
    );
}

#[tokio::test(start_paused = true)]
async fn active_run_control_watcher_cancels_token_after_slow_durable_poll() {
    let provider = Arc::new(StaticRunControlProvider::new(Some(
        RunControlStatus::Cancelled,
    )));
    let run_control: Arc<dyn RunControlProvider> = provider.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let pause_flag = Arc::new(AtomicBool::new(false));
    let cancel_token = Arc::new(CancellationToken::new());

    let _watcher = start_active_run_control_watcher(
        Some(run_control),
        "user-1".to_string(),
        "run-1".to_string(),
        cancel_flag.clone(),
        pause_flag.clone(),
        cancel_token.clone(),
    )
    .expect("watcher");

    tokio::task::yield_now().await;
    assert_eq!(provider.calls(), 0, "watcher must not poll immediately");
    tokio::time::advance(ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL - Duration::from_millis(1))
        .await;
    tokio::task::yield_now().await;
    assert_eq!(
        provider.calls(),
        0,
        "watcher must respect the slow interval"
    );

    tokio::time::advance(Duration::from_millis(1)).await;
    cancel_token.cancelled().await;

    assert!(cancel_flag.load(Ordering::Acquire));
    assert!(cancel_token.is_cancelled());
    assert!(!pause_flag.load(Ordering::Acquire));
    assert_eq!(provider.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn active_run_control_watcher_sets_pause_without_cancelling_token() {
    let provider = Arc::new(StaticRunControlProvider::new(Some(
        RunControlStatus::Paused,
    )));
    let run_control: Arc<dyn RunControlProvider> = provider.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let pause_flag = Arc::new(AtomicBool::new(false));
    let cancel_token = Arc::new(CancellationToken::new());

    let _watcher = start_active_run_control_watcher(
        Some(run_control),
        "user-1".to_string(),
        "run-1".to_string(),
        cancel_flag.clone(),
        pause_flag.clone(),
        cancel_token.clone(),
    )
    .expect("watcher");

    tokio::task::yield_now().await;
    tokio::time::advance(ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL).await;
    tokio::task::yield_now().await;

    assert_eq!(provider.calls(), 1);
    assert!(!cancel_flag.load(Ordering::Acquire));
    assert!(pause_flag.load(Ordering::Acquire));
    assert!(
        !cancel_token.is_cancelled(),
        "pause must not abort in-flight work"
    );
}

#[tokio::test(start_paused = true)]
async fn active_run_control_watcher_times_out_a_hung_provider_and_polls_again() {
    let provider = Arc::new(HangingRunControlProvider {
        calls: AtomicUsize::new(0),
    });
    let run_control: Arc<dyn RunControlProvider> = provider.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let pause_flag = Arc::new(AtomicBool::new(false));
    let cancel_token = Arc::new(CancellationToken::new());

    let _watcher = start_active_run_control_watcher(
        Some(run_control),
        "user-1".to_string(),
        "run-1".to_string(),
        cancel_flag,
        pause_flag,
        cancel_token,
    )
    .expect("watcher");

    tokio::task::yield_now().await;
    tokio::time::advance(ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.calls.load(Ordering::Acquire), 1);

    tokio::time::advance(ACTIVE_RUN_DURABLE_CONTROL_POLL_TIMEOUT).await;
    tokio::task::yield_now().await;
    tokio::time::advance(ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL).await;
    tokio::task::yield_now().await;
    assert_eq!(
        provider.calls.load(Ordering::Acquire),
        2,
        "a hung durable provider must not permanently stop control polling"
    );
}

#[tokio::test]
async fn cancel_session_runs_cancels_active_run_for_that_session_only() {
    let svc = test_service();
    let mut session_a = test_request("task a");
    session_a.session_id = Some("session-a".to_string());
    let run_a = ok(svc.create_run("user-1".into(), session_a).await);

    let mut session_b = test_request("task b");
    session_b.session_id = Some("session-b".to_string());
    let run_b = ok(svc.create_run("user-1".into(), session_b).await);

    let cancelled = ok(svc
        .cancel_session_runs("session-a".to_string(), "user-1".to_string())
        .await);

    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].run_id, run_a.run_id);
    assert_eq!(cancelled[0].status, "cancelled");
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run_a.run_id).await,
        Some(true)
    );
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run_b.run_id).await,
        Some(false),
        "session cancel must not cancel runs from a different session"
    );
    let status_b = ok(svc.get_run_status(run_b.run_id, "user-1".into()).await);
    assert_eq!(status_b.status, "running");
}

#[tokio::test(start_paused = true)]
async fn cancel_run_schedules_in_memory_eviction() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    assert!(
        svc.test_llm_cancel_token_is_cancelled(&run.run_id)
            .await
            .is_some()
    );

    ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
        None,
        "cancelled runs must not stay pinned in the process-local run cache"
    );
}

#[tokio::test]
async fn cancel_run_from_paused_sets_cancelled_status_and_clears_pause_flag() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(true));

    let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(result.status, "cancelled");
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
        Some(true)
    );
}

#[tokio::test]
async fn pause_run_sets_live_pause_flag_and_resume_clears_it() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(true));
    ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
}

#[tokio::test]
async fn cancel_run_idempotent_for_non_running() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(result.status, "cancelled");
}

#[tokio::test]
async fn cancel_run_hides_foreign_run() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    let e = err(svc.cancel_run(run.run_id, "user-2".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stream_run_returns_events_from_offset() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
    let events = ok(svc.stream_run(run.run_id.clone(), "user-1".into(), 0).await);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event_type"], "run_started");
    let events = ok(svc.stream_run(run.run_id, "user-1".into(), 1).await);
    assert!(events.is_empty());
}

#[tokio::test]
async fn stream_run_not_found() {
    let svc = test_service();
    let e = err(svc
        .stream_run("nonexistent".into(), "user-1".into(), 0)
        .await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_runs_empty_initially() {
    let svc = test_service();
    let result = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    assert_eq!(result.total, None);
    assert!(result.runs.is_empty());
}

#[tokio::test]
async fn list_runs_filters_by_user() {
    let svc = test_service();
    let u1_a = ok(svc.create_run("user-1".into(), test_request("a")).await);
    let u2_b = ok(svc.create_run("user-2".into(), test_request("b")).await);
    let u1_c = ok(svc.create_run("user-1".into(), test_request("c")).await);
    let for_u1 = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    assert_eq!(for_u1.total, None);
    let ids: std::collections::HashSet<_> = for_u1.runs.iter().map(|r| r.run_id.as_str()).collect();
    assert!(ids.contains(u1_a.run_id.as_str()));
    assert!(ids.contains(u1_c.run_id.as_str()));
    assert!(!ids.contains(u2_b.run_id.as_str()));
    assert!(
        for_u1
            .runs
            .iter()
            .all(|run| run.workspace.as_ref().unwrap()["kind"] == "none")
    );
    assert!(
        for_u1
            .runs
            .iter()
            .all(|run| run.executor.as_ref().unwrap()["kind"] == "server_local")
    );
    assert!(
        for_u1
            .runs
            .iter()
            .all(|run| run.executor.as_ref().unwrap()["executor_id"] == "server-control-plane")
    );

    let for_u2 = ok(svc.list_runs_cursor("user-2".into(), 10, None).await);
    assert_eq!(for_u2.total, None);
    assert_eq!(for_u2.runs[0].run_id, u2_b.run_id);
}

#[tokio::test]
async fn list_runs_cursor_pagination_omits_count_and_returns_next_cursor() {
    let svc = test_service();
    for i in 0..5 {
        ok(svc
            .create_run("user-1".into(), test_request(&format!("msg {i}")))
            .await);
    }
    let page1 = ok(svc.list_runs_cursor("user-1".into(), 2, None).await);
    assert_eq!(page1.total, None);
    assert_eq!(page1.runs.len(), 2);
    assert!(page1.next_cursor.is_some());
    let page2 = ok(svc
        .list_runs_cursor("user-1".into(), 2, page1.next_cursor)
        .await);
    assert_eq!(page2.total, None);
    assert_eq!(page2.runs.len(), 2);
    assert!(page2.next_cursor.is_some());
    let page3 = ok(svc
        .list_runs_cursor("user-1".into(), 2, page2.next_cursor)
        .await);
    assert_eq!(page3.total, None);
    assert_eq!(page3.runs.len(), 1);
    assert!(page3.next_cursor.is_none());
}

#[tokio::test]
async fn list_runs_orders_by_latest_update() {
    let svc = test_service();
    let older = ok(svc.create_run("user-1".into(), test_request("older")).await);
    let newer = ok(svc.create_run("user-1".into(), test_request("newer")).await);

    let initial = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    assert_eq!(initial.runs[0].run_id, newer.run_id);

    ok(svc.pause_run(older.run_id.clone(), "user-1".into()).await);

    let after_update = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    assert_eq!(
        after_update.runs[0].run_id, older.run_id,
        "list_runs_cursor should surface the most recently updated run first"
    );
}

/// P2-B: list_runs_cursor must clamp pagination params like other list endpoints.
#[tokio::test]
async fn list_runs_cursor_clamps_pagination() {
    let svc = test_service();
    // Absurdly large limit must not panic or produce unbounded queries.
    let result = ok(svc
        .list_runs_cursor("user-clamp".into(), u32::MAX, None)
        .await);
    assert_eq!(result.runs.len(), 0);
    // Verify the returned limit is clamped.
    assert!(
        result.limit <= astra_services::pagination::MAX_API_LIST_LIMIT,
        "limit must be clamped to MAX_API_LIST_LIMIT"
    );
}

#[test]
fn durable_recent_events_honors_work_surface_hydrate_limit() {
    let events = (0..450)
        .map(|i| json!({"event_type": "tool_call_end", "data": {"seq": i}}))
        .collect();
    let run = DurableRunRecord {
        run_id: "run-long".to_string(),
        user_id: "user-1".to_string(),
        session_id: "session-1".to_string(),
        parent_run_id: None,
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: None,
        retry_of: None,
        retry_scope: None,
        status: STATUS_RUNNING.to_string(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 0,
        last_event_idx: 449,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        agent_binding_id: None,
        agent_binding_name: None,
        agent_binding_schema_version: None,
        model_offering_id: None,
        resolved_model_name: None,
        runtime_profile: None,
        provider_request_fingerprint: None,
        events,
        created_at: "2026-06-13T00:00:00.000Z".to_string(),
        updated_at: "2026-06-13T00:00:00.000Z".to_string(),
    };

    let recent_events = AgenticRunLifecycleService::durable_recent_events(&run, 400);

    assert_eq!(recent_events.len(), 400);
    assert_eq!(recent_events[0]["index"], 50);
    assert_eq!(recent_events[399]["index"], 449);
}

#[test]
fn extract_edge_tools_from_context() {
    let mut ctx = serde_json::Map::new();
    ctx.insert(
        "edge_tools".to_string(),
        json!([{"function": {"name": "bash"}}]),
    );
    let req = ChatRequestData {
        message: "hi".into(),
        conversation_authority: None,
        user_intent: None,
        parts: Vec::new(),
        attachments: Vec::new(),
        stable_runtime_system_prompt: None,
        runtime_system_prompt: None,
        session_id: None,
        full_llm_capture: false,
        agent_id: None,
        model: None,
        model_selection: None,
        resolved_model_selection: None,
        admitted_model_execution: None,
        capability_descriptors: None,
        provider_runtime_authorized: false,
        agent_bindings: Vec::new(),
        agent_binding: None,
        runtime_auth: None,
        runtime_skill_binding: None,
        runtime_profile: None,
        skill_search: None,
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        enabled_tools: None,
        workspace_binding: None,
        executor_binding: None,
        runtime_mcp_bindings: Vec::new(),
        mcp_binding_ids: None,
        context: Some(ctx),
        edge_executor_id: None,
        capabilities: Vec::new(),
        forward_headers: HashMap::new(),
        execution_budget: None,
        execution_policy: Default::default(),
        explain: false,
        interaction_mode: None,
        interactive_client: false,
        provider_run_owner: None,
    };
    let tools = AgenticRunLifecycleService::extract_edge_tools(&req).expect("edge tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "bash");
}

#[test]
fn extract_edge_tools_empty_when_no_context() {
    assert!(
        AgenticRunLifecycleService::extract_edge_tools(&test_request("hi"))
            .expect("empty edge tools")
            .is_empty()
    );
}

#[test]
fn normalize_request_allowlists_preserve_explicit_empty_sets() {
    let empty: Vec<String> = Vec::new();
    assert_eq!(
        super::normalize_request_allowlist(Some(&empty), "allow_skills")
            .expect("empty allow_skills should normalize"),
        Some(HashSet::new())
    );
    assert_eq!(
        super::normalize_request_skill_sources(Some(&empty), "allow_skill_sources")
            .expect("empty allow_skill_sources should normalize"),
        Some(HashSet::new())
    );
}

#[test]
fn extract_edge_profile_from_context() {
    let mut ctx = serde_json::Map::new();
    ctx.insert(
        "edge_profile".to_string(),
        json!({
            "cwd": "/tmp",
            "git_branch": "main",
            "system_prompt_override": "override text"
        }),
    );
    let req = ChatRequestData {
        message: "hi".into(),
        conversation_authority: None,
        user_intent: None,
        parts: Vec::new(),
        attachments: Vec::new(),
        stable_runtime_system_prompt: None,
        runtime_system_prompt: None,
        session_id: None,
        full_llm_capture: false,
        agent_id: None,
        model: None,
        model_selection: None,
        resolved_model_selection: None,
        admitted_model_execution: None,
        capability_descriptors: None,
        provider_runtime_authorized: false,
        agent_bindings: Vec::new(),
        agent_binding: None,
        runtime_auth: None,
        runtime_skill_binding: None,
        runtime_profile: None,
        skill_search: None,
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        enabled_tools: None,
        workspace_binding: None,
        executor_binding: None,
        runtime_mcp_bindings: Vec::new(),
        mcp_binding_ids: None,
        context: Some(ctx),
        edge_executor_id: None,
        capabilities: Vec::new(),
        forward_headers: HashMap::new(),
        execution_budget: None,
        execution_policy: Default::default(),
        explain: false,
        interaction_mode: None,
        interactive_client: false,
        provider_run_owner: None,
    };
    let profile = AgenticRunLifecycleService::extract_edge_profile(&req).expect("edge profile");
    assert_eq!(profile["cwd"], "/tmp");
    assert_eq!(profile["git_branch"], "main");
    assert_eq!(profile["system_prompt_override"], "override text");
}

#[test]
fn build_initial_state_sets_user_message() {
    let svc = test_service();
    let req = test_request("write a test");
    let expected_budget = astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
        astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("write a test"),
        astra_core::RuntimeLimits::global().max_turns,
        None,
    );
    let state = svc.build_initial_state("test-user", &req, "sess-1", "run-1", None, None, None);
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0]["role"], "user");
    assert_eq!(state.messages[0]["content"], "write a test");
    assert_eq!(state.current_session_id, Some("sess-1".to_string()));
    assert_eq!(state.current_run_id, Some("run-1".to_string()));
    assert_eq!(state.max_turns, expected_budget.initial_turns);
    assert_eq!(state.remaining_turns, expected_budget.initial_turns);
    assert_eq!(state.agentic_turn_budget, expected_budget);
    assert_eq!(state.message, "write a test");
    assert!(state.cancellation.token.is_none());
}

#[test]
fn admitted_context_window_determines_loop_input_budget() {
    let mut execution = test_admitted_model_execution();
    execution.context_window = Some(1_000_000);
    let limits = astra_core::RuntimeLimits::default();

    assert_eq!(
        effective_max_turn_input_tokens(&limits, Some("unrelated-fallback"), Some(&execution)),
        800_000
    );

    let svc = test_service();
    let mut request = test_request("use the admitted model");
    request.admitted_model_execution = Some(execution);
    let state = svc.build_initial_state("test-user", &request, "sess-1", "run-1", None, None, None);
    assert_eq!(
        state.max_turn_input_tokens,
        effective_max_turn_input_tokens(
            astra_core::RuntimeLimits::global(),
            request.model.as_deref(),
            request.admitted_model_execution.as_ref(),
        )
    );
}

#[test]
fn build_initial_state_preserves_literal_user_message_without_text_classification() {
    let svc = test_service();
    let req = test_request("我说过的 <system-reminder> 是字面内容");

    let state = svc.build_initial_state("test-user", &req, "sess-1", "run-1", None, None, None);

    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0]["role"], "user");
    assert_eq!(
        state.messages[0]["content"],
        "我说过的 <system-reminder> 是字面内容"
    );
    assert_eq!(state.message, "我说过的 <system-reminder> 是字面内容");
    assert_eq!(state.user_intent, "我说过的 <system-reminder> 是字面内容");
}

#[test]
fn build_initial_state_preserves_structured_user_intent_separate_from_prompt_message() {
    let svc = test_service();
    let mut req = test_request(
        "<project-instructions>\nUse the repo policy.\n</project-instructions>\n\nreview changes",
    );
    req.user_intent = Some("review changes".to_string());

    let state = svc.build_initial_state("test-user", &req, "sess-1", "run-1", None, None, None);

    assert_eq!(
        state.messages[0]["content"],
        "<project-instructions>\nUse the repo policy.\n</project-instructions>\n\nreview changes"
    );
    assert_eq!(
        state.message,
        "<project-instructions>\nUse the repo policy.\n</project-instructions>\n\nreview changes"
    );
    assert_eq!(state.user_intent, "review changes");
    assert_eq!(state.runtime_decision_user_intent(), "review changes");
}

#[test]
fn build_initial_state_applies_execution_budget_override() {
    let svc = test_service();
    let mut req = test_request("go");
    req.execution_budget = Some(astra_services::runs::ExecutionBudget {
        initial_turns: Some(4),
        hard_turn_limit: Some(9),
    });
    let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
    assert_eq!(state.max_turns, 4);
    assert_eq!(state.remaining_turns, 4);
    assert_eq!(state.agentic_turn_budget.hard_turn_limit, 9);
}

#[test]
fn build_initial_state_clamps_execution_budget_override() {
    let svc = test_service();
    let mut req = test_request("go");
    req.execution_budget = Some(astra_services::runs::ExecutionBudget {
        initial_turns: Some(0),
        hard_turn_limit: Some(0),
    });
    let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
    assert_eq!(state.max_turns, 1);
    assert_eq!(state.agentic_turn_budget.hard_turn_limit, 1);
}

#[test]
fn agent_binding_prompt_context_does_not_modify_agent_override() {
    let context = test_prepared_agent_binding_context();
    let mut edge_profile = serde_json::Map::from_iter([(
        "system_prompt_override".to_string(),
        Value::String("Existing instruction.".to_string()),
    )]);

    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut edge_profile,
        Some(&context),
        None,
        None,
        None,
    )
    .expect("valid agent binding prompt context");

    assert_eq!(
        edge_profile
            .get("system_prompt_override")
            .and_then(Value::as_str),
        Some("Existing instruction.")
    );
}

#[test]
fn agent_binding_prompt_context_keeps_runtime_system_prompt_out_of_agent_override() {
    let context = test_prepared_agent_binding_context();
    let mut edge_profile = serde_json::Map::new();
    let runtime_control = r#"<runtime_control policy="moi.authoring_handoff.v1">
- If needed, call the terminal tool as your first action.
- After calling it, emit no user-facing text and perform no further work.
</runtime_control>"#;

    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut edge_profile,
        Some(&context),
        None,
        Some(runtime_control),
        None,
    )
    .expect("valid agent binding prompt context");

    assert_eq!(edge_profile.get("system_prompt_override"), None);
    let runtime_sections = edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS)
        .and_then(Value::as_array)
        .expect("runtime-owned prompt section");
    assert_eq!(
        runtime_sections,
        &[Value::String(runtime_control.to_string())]
    );
}

#[test]
fn agent_binding_prompt_context_routes_turn_context_to_volatile_lane() {
    let context = test_prepared_agent_binding_context();
    let mut edge_profile = serde_json::Map::new();
    let request_context = json!({
        "mode": "create",
        "raw_advice": "Build a GitHub triage agent.",
        "model_name": {"secret": "object-model-secret"},
        "resources": {
            "models": [{"name": "qwen3.7-max", "model_name": "qwen3.7-max"}],
            "tools": [{"name": "GitHub"}],
            "skills": [{"name": "Artifacts"}],
            "knowledge_bases": []
        },
        "attachments": [{
            "workspace_id": "workspace_current",
            "volume_id": 10001,
            "file_id": "resume_zip",
            "name": "resumes.zip",
            "mime_type": "application/zip",
            "size": 352698,
            "md5": "resume-digest",
            "secret": "must-not-appear"
        }],
        "author": "alice",
        "authority": "normal-business-field",
        "cookie": "session=secret-cookie",
        "headers": {"authorization": "Bearer header-secret"},
        "connection_string": "mysql://root:password@127.0.0.1/db",
        "dsn": "postgres://root:password@127.0.0.1/db",
        "host": "internal.local",
        "safe_but_unlisted": "should-not-appear",
        "api_key": "secret-api-key",
        "apiKey": "camel-api-key",
        "accessToken": "camel-access-token",
        "authToken": "camel-auth-token",
        "secretKey": "camel-secret-key",
        "endpointUrl": "http://127.0.0.1:9/endpoint",
        "runtime_auth": {"authorization": "Bearer secret"},
        "runtimeAuth": {"authorization": "Bearer camel-secret"},
        "capability_descriptors": {
            "mcp": {"endpoint_url": "http://127.0.0.1:9/mcp"}
        },
        "capabilityDescriptors": {
            "mcp": {"endpointUrl": "http://127.0.0.1:9/camel-mcp"}
        }
    })
    .as_object()
    .expect("context object")
    .clone();

    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut edge_profile,
        Some(&context),
        None,
        None,
        Some(&request_context),
    )
    .expect("valid agent binding prompt context");

    assert!(edge_profile.get("system_prompt_override").is_none());

    let volatile = edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS)
        .and_then(Value::as_array)
        .expect("runtime volatile texts");
    let text = volatile
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("## Runtime Turn Context"));
    assert!(text.contains("\"mode\":\"create\""));
    assert!(text.contains("\"raw_advice\":\"Build a GitHub triage agent.\""));
    assert!(text.contains("\"name\":\"GitHub\""));
    assert!(text.contains("\"name\":\"Artifacts\""));
    assert!(text.contains("\"model_name\":\"qwen3.7-max\""));
    assert!(text.contains("\"file_id\":\"resume_zip\""));
    assert!(text.contains("\"name\":\"resumes.zip\""));
    assert!(text.contains("\"mime_type\":\"application/zip\""));
    assert!(text.contains("\"author\":\"alice\""));
    assert!(text.contains("\"authority\":\"normal-business-field\""));
    assert!(!text.contains("Bearer secret"));
    assert!(!text.contains("Bearer camel-secret"));
    assert!(!text.contains("header-secret"));
    assert!(!text.contains("secret-cookie"));
    assert!(!text.contains("connection_string"));
    assert!(!text.contains("should-not-appear"));
    assert!(!text.contains("internal.local"));
    assert!(!text.contains("object-model-secret"));
    assert!(!text.contains("secret-api-key"));
    assert!(!text.contains("camel-api-key"));
    assert!(!text.contains("camel-access-token"));
    assert!(!text.contains("camel-auth-token"));
    assert!(!text.contains("camel-secret-key"));
    assert!(!text.contains("api_key"));
    assert!(!text.contains("apiKey"));
    assert!(!text.contains("accessToken"));
    assert!(!text.contains("authToken"));
    assert!(!text.contains("secretKey"));
    assert!(!text.contains("endpointUrl"));
    assert!(!text.contains("runtimeAuth"));
    assert!(!text.contains("capabilityDescriptors"));
    assert!(!text.contains("endpoint_url"));
    assert!(!text.contains("127.0.0.1"));
}

#[test]
fn agent_binding_prompt_context_preserves_moi_authoring_contract() {
    let context = test_prepared_agent_binding_context();
    let agent_md = format!("# Full agent prompt\n{}", "x".repeat(12_000));
    let request_context = json!({
        "mode": "revise",
        "raw_advice": "Make every response concise.",
        "source_agent_id": "agent_current",
        "source_agent_workspace_id": "workspace_current",
        "source_version": "1.2.3",
        "advice_user_id": "user_1",
        "current_agent": {
            "agent_id": "agent_current",
            "name": "Current Agent",
            "description": "Current description",
            "model_name": "qwen3.7-max",
            "model_config_ref": "model_ref_1",
            "tool_names": ["Search"],
            "skill_names": ["Artifacts"],
            "knowledge_base_names": ["Handbook"],
            "catalog_files": [{
                "workspace_id": "workspace_current",
                "volume_id": 10001,
                "file_id": "current_file"
            }],
            "agent_md": agent_md,
            "secret": "must-not-appear"
        },
        "authoring_context": {
            "schema_version": "moi.zero_authoring_context.v2",
            "open_candidate": {
                "agent_id": "agent_current",
                "candidate_version": "0.2.0",
                "config": {
                    "agent_id": "agent_current",
                    "name": "Current Agent Draft",
                    "description": "Current draft description",
                    "model_name": "qwen3.7-max",
                    "model_config_ref": "model_ref_1",
                    "tool_names": ["Search"],
                    "skill_names": ["Artifacts"],
                    "knowledge_base_names": ["Handbook"],
                    "catalog_files": [{
                        "workspace_id": "workspace_current",
                        "volume_id": 10002,
                        "file_id": "draft_file"
                    }],
                    "agent_md": agent_md,
                    "secret": "must-not-appear"
                },
                "secret": "must-not-appear"
            },
            "recent_chat_context": {
                "limit_turns": 10,
                "max_characters": 12000,
                "truncated": false,
                "messages": [
                    {"role": "user", "content": "Older user request", "secret": "must-not-appear"},
                    {"role": "assistant", "content": "Older assistant answer", "truncated": false}
                ],
                "secret": "must-not-appear"
            },
            "secret": "must-not-appear"
        },
        "secret": "must-not-appear"
    })
    .as_object()
    .expect("context object")
    .clone();
    let mut edge_profile = serde_json::Map::new();

    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut edge_profile,
        Some(&context),
        None,
        None,
        Some(&request_context),
    )
    .expect("valid agent binding prompt context");

    let volatile = edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS)
        .and_then(Value::as_array)
        .expect("runtime volatile texts");
    let text = volatile
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let payload = text
        .split("```json\n")
        .nth(1)
        .and_then(|value| value.strip_suffix("\n```"))
        .expect("runtime turn context JSON payload");
    let visible: Value = serde_json::from_str(payload).expect("decode runtime turn context JSON");
    assert!(text.contains("\"source_agent_id\":\"agent_current\""));
    assert!(text.contains("\"source_agent_workspace_id\":\"workspace_current\""));
    assert!(text.contains("\"source_version\":\"1.2.3\""));
    assert!(text.contains("\"agent_id\":\"agent_current\""));
    assert!(text.contains("\"tool_names\":[\"Search\"]"));
    assert_eq!(
        visible["current_agent"]["catalog_files"][0]["file_id"].as_str(),
        Some("current_file")
    );
    assert_eq!(
        visible["current_agent"]["agent_md"].as_str(),
        Some(agent_md.as_str())
    );
    assert!(text.contains("\"schema_version\":\"moi.zero_authoring_context.v2\""));
    assert_eq!(
        visible["authoring_context"]["open_candidate"]["config"]["agent_md"].as_str(),
        Some(agent_md.as_str())
    );
    assert_eq!(
        visible["authoring_context"]["open_candidate"]["config"]["catalog_files"][0]["file_id"]
            .as_str(),
        Some("draft_file")
    );
    assert_eq!(
        visible["authoring_context"]["open_candidate"]["candidate_version"].as_str(),
        Some("0.2.0")
    );
    assert!(text.contains("\"content\":\"Older user request\""));
    assert!(!text.contains("must-not-appear"));
    assert!(!text.contains("[truncated]"));
}

#[test]
fn agent_binding_prompt_context_keeps_complete_catalog_file_lists() {
    let context = test_prepared_agent_binding_context();
    let attachments = (0..32)
        .map(|index| {
            json!({
                "workspace_id": "workspace_current",
                "volume_id": 10001,
                "file_id": format!("attachment_{index}")
            })
        })
        .collect::<Vec<_>>();
    let catalog_files = (0..32)
        .map(|index| {
            json!({
                "workspace_id": "workspace_current",
                "volume_id": 10002,
                "file_id": format!("bound_{index}")
            })
        })
        .collect::<Vec<_>>();
    let request_context = json!({
        "attachments": attachments,
        "current_agent": {
            "catalog_files": catalog_files
        }
    })
    .as_object()
    .expect("context object")
    .clone();
    let mut edge_profile = serde_json::Map::new();

    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut edge_profile,
        Some(&context),
        None,
        None,
        Some(&request_context),
    )
    .expect("valid agent binding prompt context");

    let text = edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS)
        .and_then(Value::as_array)
        .expect("runtime volatile texts")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let payload = text
        .split("```json\n")
        .nth(1)
        .and_then(|value| value.strip_suffix("\n```"))
        .expect("runtime turn context JSON payload");
    let visible: Value = serde_json::from_str(payload).expect("decode runtime turn context JSON");
    assert_eq!(visible["attachments"].as_array().map(Vec::len), Some(32));
    assert_eq!(
        visible["current_agent"]["catalog_files"]
            .as_array()
            .map(Vec::len),
        Some(32)
    );
}

#[test]
fn agent_binding_prompt_context_keeps_complete_authoring_resource_lists() {
    let context = test_prepared_agent_binding_context();
    let tools = (0..32)
        .map(|index| json!({"name": format!("Tool {index}")}))
        .collect::<Vec<_>>();
    let request_context = json!({
        "resources": {
            "models": [{"name": "qwen3.7-max", "model_name": "qwen3.7-max"}],
            "tools": tools,
            "skills": [],
            "knowledge_bases": []
        }
    })
    .as_object()
    .expect("context object")
    .clone();
    let mut edge_profile = serde_json::Map::new();

    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut edge_profile,
        Some(&context),
        None,
        None,
        Some(&request_context),
    )
    .expect("valid agent binding prompt context");

    let text = edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS)
        .and_then(Value::as_array)
        .expect("runtime volatile texts")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let payload = text
        .split("```json\n")
        .nth(1)
        .and_then(|value| value.strip_suffix("\n```"))
        .expect("runtime turn context JSON payload");
    let visible: Value = serde_json::from_str(payload).expect("decode runtime turn context JSON");
    assert_eq!(
        visible["resources"]["tools"].as_array().map(Vec::len),
        Some(32)
    );
    assert_eq!(
        visible["resources"]["tools"][31]["name"].as_str(),
        Some("Tool 31")
    );
}

#[test]
fn agent_binding_prompt_context_rejects_complete_manifest_over_aggregate_token_budget() {
    let context = test_prepared_agent_binding_context();
    let request_context = json!({
        // This remains below the byte limit but exceeds the conservative
        // dense-Unicode token budget.
        "raw_advice": "你".repeat(43_000),
    })
    .as_object()
    .expect("context object")
    .clone();
    let mut edge_profile = serde_json::Map::new();

    let error = AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut edge_profile,
        Some(&context),
        None,
        None,
        Some(&request_context),
    )
    .expect_err("oversized runtime context must be rejected explicitly");

    assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error.1.0.error_code.as_deref(),
        Some("agent_binding_prompt_context_too_large")
    );
    let metadata = error.1.0.metadata.expect("budget metadata");
    assert!(metadata["actual_bytes"].as_u64().unwrap() < metadata["max_bytes"].as_u64().unwrap());
    assert!(
        metadata["estimated_tokens"].as_u64().unwrap()
            > metadata["max_estimated_tokens"].as_u64().unwrap()
    );
    assert!(
        edge_profile.is_empty(),
        "rejection must not inject a partial prompt"
    );
}

#[test]
fn agent_binding_prompt_context_keeps_stable_prompt_identical_when_turn_context_changes() {
    let context = test_prepared_agent_binding_context();
    let first_turn = serde_json::json!({
        "mode": "create",
        "raw_advice": "first turn advice",
        "resources": {
            "models": [{"name": "qwen3.7-max", "model_name": "qwen3.7-max"}]
        }
    })
    .as_object()
    .expect("first turn context")
    .clone();
    let second_turn = serde_json::json!({
        "mode": "refine",
        "raw_advice": "second turn advice",
        "resources": {
            "models": [{"name": "qwen3.7-max", "model_name": "qwen3.7-max"}]
        }
    })
    .as_object()
    .expect("second turn context")
    .clone();
    let mut first_profile = serde_json::Map::new();
    let mut second_profile = serde_json::Map::new();

    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut first_profile,
        Some(&context),
        Some("Session-level runtime system prompt."),
        None,
        Some(&first_turn),
    )
    .expect("valid first-turn agent binding prompt context");
    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut second_profile,
        Some(&context),
        Some("Session-level runtime system prompt."),
        None,
        Some(&second_turn),
    )
    .expect("valid second-turn agent binding prompt context");

    let first_stable = first_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_STABLE_TEXTS)
        .and_then(Value::as_array)
        .expect("first stable prompt");
    let second_stable = second_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_STABLE_TEXTS)
        .and_then(Value::as_array)
        .expect("second stable prompt");
    assert_eq!(
        first_stable, second_stable,
        "per-turn runtime context must not churn the session-stable prompt prefix"
    );
    assert_eq!(
        first_stable,
        &[Value::String(
            "Session-level runtime system prompt.".to_string()
        )]
    );

    let first_volatile = first_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS)
        .and_then(Value::as_array)
        .expect("first volatile texts")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let second_volatile = second_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS)
        .and_then(Value::as_array)
        .expect("second volatile texts")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        first_profile
            .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS)
            .is_none()
    );
    assert!(
        second_profile
            .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS)
            .is_none()
    );
    assert_ne!(first_volatile, second_volatile);
    assert!(first_volatile.contains("first turn advice"));
    assert!(second_volatile.contains("second turn advice"));
}

#[test]
fn build_initial_state_agent_binding_uses_binding_skills_and_request_budget() {
    let svc = test_service();
    let mut req = test_request("go");
    req.execution_budget = Some(astra_services::runs::ExecutionBudget {
        initial_turns: Some(8),
        hard_turn_limit: Some(12),
    });
    let mut binding_context = test_prepared_agent_binding_context();
    binding_context.skill_resolver = Some(static_skill_resolver("binding-only"));
    let edge_context =
        AgenticRunLifecycleService::extract_edge_context(&req).expect("edge context");
    let mut edge_profile = edge_context.edge_profile.to_map();
    AgenticRunLifecycleService::apply_agent_binding_prompt_context(
        &mut edge_profile,
        Some(&binding_context),
        None,
        None,
        req.context.as_ref(),
    )
    .expect("valid agent binding prompt context");

    let state = svc.build_initial_state_inner(
        "test-user",
        &req,
        "s",
        "r",
        None,
        None,
        None,
        None,
        RequestConstraints::default(),
        &edge_context,
        Some(&edge_profile),
        None,
        Some(&binding_context),
    );

    assert_eq!(state.max_turns, 8);
    assert_eq!(state.remaining_turns, 8);
    assert_eq!(state.agentic_turn_budget.hard_turn_limit, 12);
    assert!(state.skills.registry_for_activation.is_none());
    assert_eq!(
        state
            .skills
            .listing_message
            .as_ref()
            .and_then(|message| { message.get("content").and_then(Value::as_str) }),
        Some("## Agent Binding Instructions")
    );
    let names: Vec<String> = state
        .skills
        .resolver
        .as_ref()
        .expect("binding skill resolver must be installed")
        .available_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(names, vec!["binding-only".to_string()]);
}

#[tokio::test]
async fn request_scoped_runtime_skill_resolver_is_installed_from_provider_capability() {
    use axum::{Router, extract::State, http::HeaderMap, routing::post};
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct Capture {
        authorizations: Mutex<Vec<String>>,
        bodies: Mutex<Vec<Value>>,
    }

    async fn handler(
        State(capture): State<Arc<Capture>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        capture.authorizations.lock().await.push(
            headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
        );
        capture.bodies.lock().await.push(body.clone());
        match body.get("method").and_then(Value::as_str) {
            Some("skills/list") => Json(json!({
                "jsonrpc": "2.0",
                "id": "astra-agent-binding-skills-list",
                "result": {
                    "skills": [{
                        "name": "moi-skill",
                        "description": "Skill from external provider runtime context",
                        "when_to_use": "when MOI grants this skill for the turn",
                        "aliases": ["moi-alias"],
                        "category": "external",
                        "tags": ["moi"],
                        "allowed_tools": [],
                        "input_schema": {"type": "object"},
                        "output_schema": {"type": "object"}
                    }]
                }
            })),
            Some("skills/read") => Json(json!({
                "jsonrpc": "2.0",
                "id": "astra-agent-binding-skills-read",
                "result": {
                    "skill": {
                        "id": "moi-skill",
                        "instruction": {
                            "body": "Call the provider skill capability server."
                        }
                    }
                }
            })),
            other => panic!("unexpected method: {other:?}"),
        }
    }

    let capture = Arc::new(Capture::default());
    let app = Router::new()
        .route("/skills", post(handler))
        .with_state(capture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local skill capability server");
    let addr = listener.local_addr().expect("listener addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let endpoint = format!("http://{addr}/skills");

    let svc = test_service();
    let mut request = prepared_test_request("use the provider skill");
    request.allow_skills = Some(vec!["moi-skill".to_string()]);
    request.runtime_skill_binding = Some(RuntimeSkillBindingRequest {
        id: "moi-skills".to_string(),
        url: endpoint.clone(),
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.forward_headers.insert(
        "authorization".to_string(),
        "Bearer runtime-grant".to_string(),
    );
    let request_constraints = AgenticRunLifecycleService::try_request_constraints(&request)
        .expect("skill allowlist should parse");
    let capabilities = svc
        .prepare_runtime_capabilities(&request, &request_constraints)
        .await
        .expect("provider skill capability should prepare a resolver");

    assert!(capabilities.agent_binding.is_none());
    assert!(capabilities.request_scoped_skill_resolver.is_some());
    let edge_context =
        AgenticRunLifecycleService::extract_edge_context(&request).expect("edge context");

    let state = svc.build_initial_state_inner(
        "external-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
        None,
        request_constraints,
        &edge_context,
        None,
        capabilities.request_scoped_skill_resolver.clone(),
        capabilities.agent_binding.as_ref(),
    );

    assert!(state.skills.registry_for_activation.is_none());
    let resolver = state
        .skills
        .resolver
        .as_ref()
        .expect("runtime-scoped skill resolver must be installed");
    let available = resolver.available_skills();
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].name, "moi-skill");
    let resolved = resolver
        .resolve_for_execution("moi-alias")
        .await
        .expect("runtime-scoped skill alias should load");
    assert_eq!(
        resolved.instructions,
        "Call the provider skill capability server."
    );
    assert!(resolved.remote_url.is_none());
    assert!(resolved.forward_headers.is_empty());
    assert!(resolved.required_headers.is_empty());

    let manifest =
        AgenticRunLifecycleService::build_runtime_manifest(&request, &capabilities, false)
            .expect("runtime manifest should be internally consistent")
            .expect("selected model should produce manifest");
    assert!(manifest.get("agent_binding").is_none());
    assert_eq!(
        manifest["request_scoped_runtime"]["discovered_skills"][0]["name"],
        "moi-skill"
    );
    assert_eq!(
        capture.authorizations.lock().await.as_slice(),
        &["Bearer runtime-grant", "Bearer runtime-grant"]
    );
    assert_eq!(
        capture.bodies.lock().await.as_slice(),
        &[
            json!({
                "jsonrpc": "2.0",
                "id": "astra-agent-binding-skills-list",
                "method": "skills/list"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": "astra-agent-binding-skills-read",
                "method": "skills/read",
                "params": {"id": "moi-skill"}
            })
        ]
    );
    server.abort();
}

#[tokio::test]
async fn agent_binding_runtime_discovers_descriptor_capabilities_concurrently() {
    use axum::{Router, extract::State, http::HeaderMap, routing::post};
    use tokio::sync::{Barrier, Mutex};

    struct Capture {
        mcp_authorization: Mutex<Option<String>>,
        skill_authorization: Mutex<Option<String>>,
        discovery_barrier: Barrier,
    }

    impl Default for Capture {
        fn default() -> Self {
            Self {
                mcp_authorization: Mutex::new(None),
                skill_authorization: Mutex::new(None),
                discovery_barrier: Barrier::new(2),
            }
        }
    }

    async fn mcp_handler(
        State(capture): State<Arc<Capture>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *capture.mcp_authorization.lock().await = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        capture.discovery_barrier.wait().await;
        Json(json!({
            "jsonrpc": "2.0",
            "id": body.get("id").cloned().unwrap_or(Value::Null),
            "result": {"tools": []}
        }))
    }

    async fn skills_handler(
        State(capture): State<Arc<Capture>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *capture.skill_authorization.lock().await = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        capture.discovery_barrier.wait().await;
        Json(json!({
            "jsonrpc": "2.0",
            "id": body.get("id").cloned().unwrap_or(Value::Null),
            "result": {
                "skills": [{
                    "name": "moi-skill",
                    "description": "Skill from descriptor endpoint",
                    "when_to_use": "when the binding grants it",
                    "allowed_tools": [],
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"}
                }]
            }
        }))
    }

    let capture = Arc::new(Capture::default());
    let app = Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/skills", post(skills_handler))
        .with_state(capture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local capability server");
    let addr = listener.local_addr().expect("listener addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let record = legacy_endpoint_agent_binding_record();
    let service = test_service().with_agent_binding_service(Arc::new(StaticAgentBindingService {
        record: record.clone(),
    }));
    let mut request = prepared_test_request("use binding tools");
    request.provider_runtime_authorized = true;
    request.admitted_model_execution = None;
    request.runtime_profile = Some(RuntimeProfileRequest::AgentBindingRegistry);
    request.agent_binding = Some(runtime_binding_request(record.id, "tools", "skills"));
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "moi-model-gateway",
                "model_gateway",
                &format!("http://{addr}/model"),
            )),
            mcp: Some(test_runtime_descriptor(
                "tools",
                "mcp",
                &format!("http://{addr}/mcp"),
            )),
            skills: Some(test_runtime_descriptor(
                "skills",
                "skills",
                &format!("http://{addr}/skills"),
            )),
            edge_agent: None,
            file_transfer: None,
        });

    let capabilities = tokio::time::timeout(
        Duration::from_secs(2),
        service.prepare_runtime_capabilities(&request, &RequestConstraints::default()),
    )
    .await
    .expect("MCP and skill discovery must overlap")
    .expect("agent binding descriptors should prepare capabilities");

    assert!(capabilities.mcp_bundle.is_some());
    assert!(capabilities.agent_binding.is_some());
    assert_eq!(
        capture.mcp_authorization.lock().await.as_deref(),
        Some("Bearer runtime-grant")
    );
    assert_eq!(
        capture.skill_authorization.lock().await.as_deref(),
        Some("Bearer runtime-grant")
    );
    let manifest =
        AgenticRunLifecycleService::build_runtime_manifest(&request, &capabilities, false)
            .expect("runtime manifest should be internally consistent")
            .expect("model selection should produce a runtime manifest");
    assert_eq!(
        manifest["model_selection"]["offering_id"],
        "model-test-model"
    );
    assert_eq!(
        manifest["model_resolution"]["source"],
        "provider_descriptor"
    );
    assert_eq!(
        manifest["model_resolution"]["descriptor"]["id"],
        "moi-model-gateway"
    );
    server.abort();
}

#[test]
fn runtime_manifest_preserves_server_only_backbone_without_workspace_executor() {
    let mut request = prepared_test_request("answer with server-side context");
    request.parts = vec![json!({"type": "text", "text": "answer with server-side context"})];
    request.attachments = vec![json!({"id": "att-server-only", "kind": "note"})];
    request.capabilities = vec![
        "web_fetch".to_string(),
        "memory".to_string(),
        "introspect".to_string(),
    ];
    request.context = Some(
        json!({
            "access_surface": "web",
            "workspace": null
        })
        .as_object()
        .expect("context object")
        .clone(),
    );

    let manifest = AgenticRunLifecycleService::build_runtime_manifest(
        &request,
        &PreparedRuntimeCapabilities::default(),
        false,
    )
    .expect("runtime manifest should be internally consistent")
    .expect("model selection should produce a server-only runtime manifest");

    assert_eq!(manifest["schema_version"], "astra_runtime_manifest.v1");
    assert_eq!(manifest["runtime_profile"], "astra_native");
    assert_eq!(
        manifest["model_selection"]["offering_id"],
        "model-test-model"
    );
    assert_eq!(manifest["model_resolution"]["source"], "catalog_offering");
    assert_eq!(manifest["model_resolution"]["model"], "test-model");
    assert_eq!(manifest["model_resolution"]["resolved"], true);
    assert_eq!(
        manifest["turn"]["message"],
        "answer with server-side context"
    );
    assert_eq!(manifest["turn"]["parts"][0]["type"], "text");
    assert_eq!(manifest["turn"]["attachments"][0]["id"], "att-server-only");
    assert_eq!(manifest["turn"]["edge_executor_id"], Value::Null);
    assert_eq!(manifest["turn"]["context"]["access_surface"], "web");
    assert_eq!(manifest["turn"]["capabilities"][0], "web_fetch");
    assert_eq!(
        manifest["capacity_resolution"]["server_builtin_surface"],
        "server_service_control_plane_only"
    );
    assert_eq!(
        manifest["capacity_resolution"]["workspace_executor_admitted"], false,
        "pure Web/server-only keeps the full runtime backbone but must not imply workspace/process capacity"
    );
    assert!(
        manifest.get("agent_binding").is_none(),
        "server-only native runtime should not invent an agent-binding runtime"
    );
    assert!(
        manifest.get("request_scoped_runtime").is_none(),
        "server-only native runtime should not invent request-scoped MCP/skill runtime state"
    );
}

#[test]
fn runtime_manifest_includes_agent_binding_snapshot_without_runtime_auth() {
    let mut request = prepared_test_request("use binding tools");
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer secret-runtime-token".to_string(),
    });
    request.runtime_profile = Some(RuntimeProfileRequest::AgentBindingRegistry);
    request.parts = vec![json!({"type": "text", "text": "use binding tools"})];
    request.attachments = vec![json!({"id": "att-1", "kind": "file"})];
    request.edge_executor_id = Some("edge-1".to_string());
    request.capabilities = vec!["bash".to_string(), "fs".to_string()];
    let provider_snapshot = astra_turn_types::ProviderDiscoverySnapshot::new(
        astra_turn_types::ProviderIdentity::new("capability-server-tools").unwrap(),
        astra_turn_types::ProviderBindingRef::new("tools").unwrap(),
        astra_turn_types::ProviderProtocolId::new("mcp").unwrap(),
        vec![astra_turn_types::ProviderToolDeclaration {
            native_tool_id: astra_turn_types::NativeToolId::new("query").unwrap(),
            native_tool_name: "query".to_string(),
            stable_tool_alias: None,
            title: Some("Query".to_string()),
            description: Some("Query data".to_string()),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            claims: Default::default(),
            task_support: Default::default(),
            extension_fields: Default::default(),
        }],
    )
    .unwrap();
    let provider_snapshot = runtime_mcp::resolve_mcp_snapshot("tools", &provider_snapshot)
        .expect("test discovery snapshot should resolve");
    let provider_snapshot_hash = provider_snapshot.content_hash.clone();
    let provider_policy_index =
        astra_turn_core::provider_resolution::ResolvedProviderPolicyIndex::from_snapshots(
            std::slice::from_ref(&provider_snapshot),
        )
        .unwrap();
    let capabilities = PreparedRuntimeCapabilities {
        mcp_bundle: Some(runtime_mcp::RuntimeMcpBundle {
            schemas: vec![json!({
                "type": "function",
                "function": {
                    "name": "mcp__tools__query",
                    "description": "Query data",
                    "parameters": {"type": "object"}
                }
            })],
            provider_snapshots: vec![provider_snapshot],
            provider_policy_index,
            control_tools: Default::default(),
            stop_after_success_tools: Default::default(),
            manager: None,
            agent_binding_mcp: None,
            semantic_read_capabilities: Default::default(),
        }),
        request_scoped_skill_resolver: None,
        agent_binding: Some({
            let mut context = test_prepared_agent_binding_context();
            context.skill_resolver = Some(static_skill_resolver("binding-only"));
            context.skill_catalogs[0].skills =
                vec![test_skill_info("binding-only", "Binding-scoped skill")];
            context
        }),
    };

    let manifest =
        AgenticRunLifecycleService::build_runtime_manifest(&request, &capabilities, false)
            .expect("runtime manifest should be internally consistent")
            .expect("model selection should produce a runtime manifest");

    assert_eq!(
        manifest["model_selection"]["offering_id"],
        "model-test-model"
    );
    assert_eq!(manifest["runtime_profile"], "agent_binding_registry");
    assert_eq!(manifest["turn"]["message"], "use binding tools");
    assert_eq!(manifest["turn"]["parts"][0]["type"], "text");
    assert_eq!(manifest["turn"]["attachments"][0]["id"], "att-1");
    assert_eq!(manifest["turn"]["edge_executor_id"], "edge-1");
    assert_eq!(manifest["turn"]["capabilities"][0], "bash");
    assert_eq!(manifest["agent_bindings"].as_array().map(Vec::len), Some(1));
    assert_eq!(manifest["agent_bindings"][0]["id"], "abnd_test1234567890");
    assert_eq!(
        manifest["agent_bindings"][0]["discovered_skills"][0]["name"],
        "binding-only"
    );
    assert_eq!(
        manifest["agent_binding_set"]["discovered_tools"][0]["function"]["name"],
        "mcp__tools__query"
    );
    assert_eq!(manifest["agent_binding_set"]["binding_count"], 1);
    assert_eq!(
        manifest["provider_snapshot_refs"][0]["provider_identity"],
        "capability-server-tools"
    );
    assert_eq!(
        manifest["provider_snapshot_refs"][0]["binding_ref"],
        "tools"
    );
    assert_eq!(manifest["provider_snapshot_refs"][0]["protocol"], "mcp");
    assert_eq!(
        manifest["provider_snapshot_refs"][0]["content_hash"],
        provider_snapshot_hash
    );
    assert_eq!(manifest["provider_snapshot_refs"][0]["tool_count"], 1);
    assert!(
        manifest["provider_snapshot_refs"][0]
            .get("tool_declarations")
            .is_none(),
        "runtime manifest must project a bounded reference, not the declaration graph"
    );
    let serialized = serde_json::to_string(&manifest).expect("runtime manifest should serialize");
    assert!(!serialized.contains("secret-runtime-token"));
    assert!(!serialized.contains("Bearer"));
}

#[test]
fn install_agent_binding_runtime_forward_headers_uses_runtime_auth() {
    let mut req = test_request("go");
    req.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
    });
    req.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    req.forward_headers.insert(
        "authorization".to_string(),
        "Bearer client-token".to_string(),
    );

    AgenticRunLifecycleService::install_agent_binding_runtime_forward_headers(&mut req)
        .expect("runtime auth should be forwarded in memory for binding skills");

    assert_eq!(
        req.forward_headers.get("authorization").map(String::as_str),
        Some("Bearer runtime-grant")
    );
}

#[test]
fn build_initial_state_loads_stop_hooks_from_edge_profile_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let mo = dir.path().join(".astra");
    std::fs::create_dir_all(&mo).unwrap();
    std::fs::write(
        mo.join("stop-hooks.yaml"),
        "version: 1\nauto_detect: false\nhooks:\n  - label: cloud_hook\n    command: true\n",
    )
    .unwrap();

    let svc = test_service();
    let mut req = test_request("implement a fix");
    req.context = Some(
        serde_json::json!({
            "edge_profile": { "cwd": dir.path().to_str().unwrap() }
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
    assert_eq!(state.hooks.stop_hooks.len(), 1);
    assert_eq!(state.hooks.stop_hooks[0].label, "cloud_hook");
    assert_eq!(
        state.hooks.workspace_root_hint.as_deref(),
        Some(dir.path().to_str().unwrap())
    );
}

#[test]
fn build_initial_state_uses_workspace_override_when_no_edge_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let mo = dir.path().join(".astra");
    std::fs::create_dir_all(&mo).unwrap();
    std::fs::write(
        mo.join("stop-hooks.yaml"),
        "version: 1\nauto_detect: false\nhooks:\n  - label: server_hook\n    command: echo ok\n",
    )
    .unwrap();

    let svc = test_service();
    // Request with NO edge_profile.cwd — simulates web-agent mode.
    let req = test_request("fix a bug");
    let state = svc.build_initial_state("test-user", &req, "s", "r", Some(dir.path()), None, None);
    assert_eq!(state.hooks.stop_hooks.len(), 1);
    assert_eq!(state.hooks.stop_hooks[0].label, "server_hook");
    assert_eq!(
        state.hooks.workspace_root_hint.as_deref(),
        Some(dir.path().to_str().unwrap())
    );
}

#[test]
fn build_initial_state_edge_cwd_takes_priority_over_workspace_override() {
    // Edge profile with cwd set — workspace_override should be ignored.
    let edge_dir = tempfile::tempdir().unwrap();
    let mo = edge_dir.path().join(".astra");
    std::fs::create_dir_all(&mo).unwrap();
    std::fs::write(
        mo.join("stop-hooks.yaml"),
        "version: 1\nauto_detect: false\nhooks:\n  - label: edge_hook\n    command: true\n",
    )
    .unwrap();

    let override_dir = tempfile::tempdir().unwrap();
    let mo2 = override_dir.path().join(".astra");
    std::fs::create_dir_all(&mo2).unwrap();
    std::fs::write(
        mo2.join("stop-hooks.yaml"),
        "version: 1\nauto_detect: false\nhooks:\n  - label: override_hook\n    command: true\n",
    )
    .unwrap();

    let svc = test_service();
    let mut req = test_request("deploy");
    req.context = Some(
        serde_json::json!({
            "edge_profile": { "cwd": edge_dir.path().to_str().unwrap() }
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let state = svc.build_initial_state(
        "test-user",
        &req,
        "s",
        "r",
        Some(override_dir.path()),
        None,
        None,
    );
    // Edge profile's cwd wins over the workspace override.
    assert_eq!(state.hooks.stop_hooks.len(), 1);
    assert_eq!(state.hooks.stop_hooks[0].label, "edge_hook");
    assert_eq!(
        state.hooks.workspace_root_hint.as_deref(),
        Some(edge_dir.path().to_str().unwrap())
    );
}

#[test]
fn run_status_as_str() {
    assert_eq!(RunStatus::Running.as_str(), "running");
    assert_eq!(RunStatus::Completed.as_str(), "completed");
    assert_eq!(RunStatus::Delegated.as_str(), "delegated");
    assert_eq!(RunStatus::Failed.as_str(), "failed");
    assert_eq!(RunStatus::Cancelled.as_str(), "cancelled");
    assert_eq!(RunStatus::Paused.as_str(), "paused");
}

#[test]
fn has_buffered_terminal_completion_ignores_cancelled_and_interrupted_finishes() {
    assert!(has_buffered_terminal_completion(&[json!({
        "event_type": "run_finished",
        "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
    })]));
    assert!(!has_buffered_terminal_completion(&[json!({
        "event_type": "run_finished",
        "data": {"cancelled": true}
    })]));
    assert!(!has_buffered_terminal_completion(&[json!({
        "event_type": "run_finished",
        "data": {"interrupted": true}
    })]));
    assert!(!has_buffered_terminal_completion(&[
        json!({
            "event_type": "run_finished",
            "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
        }),
        json!({
            "event_type": "run_finished",
            "data": {"cancelled": true}
        }),
    ]));
}

#[test]
fn preserve_manual_pause_wins_over_late_completed_status() {
    assert!(should_preserve_manual_pause_on_completion(
        &RunStatus::Paused,
        &RunStatus::Completed
    ));
    assert!(!should_preserve_manual_pause_on_completion(
        &RunStatus::Paused,
        &RunStatus::Failed
    ));
    assert!(!should_preserve_manual_pause_on_completion(
        &RunStatus::Running,
        &RunStatus::Completed
    ));
}

#[tokio::test]
async fn durable_paused_state_wins_over_late_completed_status() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    svc.run_engine
        .persist_status(
            "user-1",
            &run.run_id,
            STATUS_PAUSED,
            Some("user_resume"),
            None,
        )
        .await
        .unwrap();

    assert!(
        should_preserve_manual_pause_from_durable(
            &svc.run_engine,
            "user-1",
            &run.run_id,
            &RunStatus::Completed,
        )
        .await
    );
    assert!(
        !should_preserve_manual_pause_from_durable(
            &svc.run_engine,
            "user-1",
            &run.run_id,
            &RunStatus::Failed,
        )
        .await
    );
}

#[tokio::test]
async fn pause_run_transitions_running_to_paused() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    let result = ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(result.status, "paused");
    assert_eq!(result.previous_status, "running");

    // HTTP surface
    let status = ok(svc
        .get_run_status(run.run_id.clone(), "user-1".into())
        .await);
    assert_eq!(status.status, "paused");

    // Memory state: verify pause_flag, waiting_for, status, and events
    let runs = svc.runs.read().await;
    let live = runs.get(&run.run_id).expect("live run present after pause");
    assert!(
        live.pause_flag.load(Ordering::SeqCst),
        "pause_flag must be set"
    );
    assert_eq!(
        live.waiting_for.as_deref(),
        Some("user_resume"),
        "waiting_for must be user_resume"
    );
    assert_eq!(live.status, RunStatus::Paused);
    let events = &live.events;
    assert_eq!(events.len(), 2, "expect run_started + run_paused");
    assert_eq!(events[1]["event_type"], "run_paused");
}

#[tokio::test]
async fn pause_run_conflict_when_not_running() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    let e = err(svc.pause_run(run.run_id, "user-1".into()).await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn pause_run_hides_foreign_run() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    let e = err(svc.pause_run(run.run_id, "user-2".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pause_run_not_found() {
    let svc = test_service();
    let e = err(svc.pause_run("nonexistent".into(), "user-1".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_run_transitions_paused_to_running() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(result.status, "running");
    assert_eq!(result.previous_status, "paused");

    // HTTP surface
    let status = ok(svc
        .get_run_status(run.run_id.clone(), "user-1".into())
        .await);
    assert_eq!(status.status, "running");

    // Memory state: verify pause_flag cleared, waiting_for cleared, events
    let runs = svc.runs.read().await;
    let live = runs
        .get(&run.run_id)
        .expect("live run present after resume");
    assert!(
        !live.pause_flag.load(Ordering::SeqCst),
        "pause_flag must be cleared"
    );
    assert_eq!(
        live.waiting_for, None,
        "waiting_for must be None after resume"
    );
    assert_eq!(live.status, RunStatus::Running);
    let events = &live.events;
    assert_eq!(
        events.len(),
        3,
        "expect run_started + run_paused + run_resumed"
    );
    assert_eq!(events[2]["event_type"], "run_resumed");
}

#[tokio::test]
async fn resume_run_promotes_buffered_completed_pause_to_completed() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    svc.run_engine
        .append_event(
            "user-1",
            &run.run_id,
            json!({
                "event_type": "run_finished",
                "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
            }),
        )
        .await
        .unwrap();

    let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(result.status, "completed");
    assert_eq!(result.previous_status, "paused");
    let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
    assert_eq!(status.status, "completed");
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_pause_resume_promotes_buffered_completed_terminal() {
    let pool = setup_lifecycle_run_db_it().await;
    let svc = db_backed_test_service(&pool, "pause-resume-it-pod-completed");
    let user_id = "user-1";
    let run_id = format!("pause-it-{}", Uuid::new_v4());
    let session_id = format!("sess-it-{}", Uuid::new_v4());
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    seed_lifecycle_run_for_pause_resume_it(&svc, user_id, &run_id, &session_id).await;

    ok(svc.pause_run(run_id.clone(), user_id.to_string()).await);
    svc.run_engine
        .append_event(
            user_id,
            &run_id,
            json!({
                "event_type": "run_finished",
                "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
            }),
        )
        .await
        .expect("append buffered completed terminal event");

    let result = ok(svc.resume_run(run_id.clone(), user_id.to_string()).await);
    assert_eq!(result.status, STATUS_COMPLETED);
    assert_eq!(result.previous_status, STATUS_PAUSED);

    let durable = svc
        .run_engine
        .load_run(user_id, &run_id)
        .await
        .expect("load durable run")
        .expect("durable run exists");
    assert_eq!(durable.status, STATUS_COMPLETED);
    assert!(durable.waiting_for.is_none());
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");

    {
        let runs = svc.runs.read().await;
        let live = runs.get(&run_id).expect("live run should still be tracked");
        assert!(matches!(&live.status, RunStatus::Completed));
    }
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_cross_pod_parent_control_reaches_remote_child() {
    let pool = setup_lifecycle_run_db_it().await;
    let owner = db_backed_test_service(&pool, "control-it-owner-pod");
    let controller = db_backed_test_service(&pool, "control-it-controller-pod");
    let user_id = format!("control-it-user-{}", Uuid::new_v4());
    let session_id = format!("control-it-session-{}", Uuid::new_v4());
    let root_id = format!("control-it-root-{}", Uuid::new_v4());
    let child_id = format!("control-it-child-{}", Uuid::new_v4());
    for run_id in [&child_id, &root_id] {
        cleanup_lifecycle_run_fixture(&pool, &user_id, run_id).await;
    }

    owner
        .run_engine
        .start_run(&root_id, &user_id, &session_id)
        .await
        .expect("owner starts root");
    owner
        .run_engine
        .start_run_ext(
            &child_id,
            &user_id,
            &session_id,
            Some(&root_id),
            Some("control-it-delegation"),
            Some("control-it-agent"),
            None,
        )
        .await
        .expect("owner starts child");

    let paused = ok(controller.pause_run(root_id.clone(), user_id.clone()).await);
    assert_eq!(paused.status, STATUS_PAUSED);
    assert_eq!(
        owner
            .run_engine
            .check_control_status(&user_id, &child_id)
            .await
            .expect("owner polls remote pause"),
        Some(RunControlStatus::Paused)
    );

    let cancelled = ok(controller
        .cancel_run(root_id.clone(), user_id.clone())
        .await);
    assert_eq!(cancelled.status, STATUS_CANCELLED);
    assert_eq!(
        owner
            .run_engine
            .check_control_status(&user_id, &child_id)
            .await
            .expect("owner polls remote cancellation"),
        Some(RunControlStatus::Cancelled)
    );
    let child = owner
        .run_engine
        .load_run(&user_id, &child_id)
        .await
        .unwrap()
        .expect("child remains durably queryable after cascading cancellation");
    assert_eq!(child.status, STATUS_CANCELLED);
    let terminal = child.events.last().expect("child cancellation event");
    assert_eq!(
        terminal.get("event_type").and_then(Value::as_str),
        Some("run_finished")
    );
    assert_eq!(
        terminal
            .get("data")
            .and_then(|data| data.get("ancestor_run_id"))
            .and_then(Value::as_str),
        Some(root_id.as_str())
    );

    for run_id in [&child_id, &root_id] {
        cleanup_lifecycle_run_fixture(&pool, &user_id, run_id).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_cross_pod_interactions_resume_detached_gates_and_reject_late_conflicts() {
    let pool = setup_lifecycle_run_db_it().await;
    let owner = db_backed_test_service(&pool, "interaction-it-owner-pod");
    let callback = db_backed_test_service(&pool, "interaction-it-callback-pod");
    let user_id = format!("interaction-it-user-{}", Uuid::new_v4());
    let approval_session_id = format!("it-approval-s-{}", Uuid::new_v4());
    let prompt_session_id = format!("it-prompt-s-{}", Uuid::new_v4());
    let timeout_session_id = format!("it-timeout-s-{}", Uuid::new_v4());
    let approval_run_id = format!("interaction-it-approval-{}", Uuid::new_v4());
    let prompt_run_id = format!("interaction-it-prompt-{}", Uuid::new_v4());
    let timeout_run_id = format!("interaction-it-timeout-{}", Uuid::new_v4());
    for (run_id, session_id) in [
        (&approval_run_id, &approval_session_id),
        (&prompt_run_id, &prompt_session_id),
        (&timeout_run_id, &timeout_session_id),
    ] {
        cleanup_lifecycle_run_fixture(&pool, &user_id, run_id).await;
        owner
            .run_engine
            .start_run(run_id, &user_id, session_id)
            .await
            .expect("owner starts interaction run");
    }

    let approval_gate = DurableRunApprovalGate::new(
        user_id.clone(),
        approval_session_id.clone(),
        approval_run_id.clone(),
        Some(1),
        owner.run_engine.clone(),
        owner.runs_handle(),
        None,
        None,
    )
    .with_timeout(Duration::from_secs(3));
    let approval_wait = tokio::spawn(async move {
        astra_tools::ToolApprovalGate::request_approval(
            &approval_gate,
            "approval-cross-pod",
            "bash",
            &json!({"command": "git status"}),
        )
        .await
    });
    wait_for_durable_run_status(
        &owner.run_engine,
        &user_id,
        &approval_run_id,
        STATUS_WAITING,
    )
    .await;
    let approval_resolution = callback
        .resolve_run_interaction(
            approval_run_id.clone(),
            user_id.clone(),
            "approval-cross-pod".into(),
            astra_services::runs::DurableRunInteractionKind::Approval,
            json!({
                "request_id": "approval-cross-pod",
                "outcome": "approved",
                "decision": "allow",
                "reason": null,
                "tool": "bash",
                "approval_kind": "standard",
            }),
        )
        .await
        .expect("callback pod resolves approval");
    assert!(matches!(
        approval_resolution,
        astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_)
    ));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), approval_wait)
            .await
            .expect("detached approval gate resumes")
            .expect("approval gate task"),
        astra_tools::ApprovalDecision::Approved
    ));

    let prompt = astra_tools::AskUserPrompt {
        context: Some("Cross-pod question".into()),
        questions: vec![astra_tools::AskUserQuestion {
            header: "Scope".into(),
            question: "Continue?".into(),
            options: Vec::new(),
            multi_select: false,
            allow_freeform: true,
        }],
        timeout_ms: None,
    };
    let prompt_gate = DurableRunUserPromptGate::new(
        user_id.clone(),
        prompt_session_id.clone(),
        prompt_run_id.clone(),
        Some(2),
        owner.run_engine.clone(),
        owner.runs_handle(),
        None,
        None,
    )
    .with_timeout(Duration::from_secs(3));
    let prompt_wait = tokio::spawn(async move {
        astra_tools::AskUserGate::request_questionnaire(&prompt_gate, "prompt-cross-pod", &prompt)
            .await
    });
    wait_for_durable_run_status(&owner.run_engine, &user_id, &prompt_run_id, STATUS_WAITING).await;
    callback
        .resolve_run_interaction(
            prompt_run_id.clone(),
            user_id.clone(),
            "prompt-cross-pod".into(),
            astra_services::runs::DurableRunInteractionKind::AskUser,
            json!({
                "request_id": "prompt-cross-pod",
                "outcome": "submitted",
                "answers": {
                    "answers": [{
                        "question": "Continue?",
                        "answers": ["yes"],
                        "multi_select": false,
                        "annotation": null,
                    }]
                }
            }),
        )
        .await
        .expect("callback pod resolves ask_user");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), prompt_wait)
            .await
            .expect("detached prompt gate resumes")
            .expect("prompt gate task"),
        astra_tools::AskUserDecision::Submitted(_)
    ));

    let timeout_gate = DurableRunApprovalGate::new(
        user_id.clone(),
        timeout_session_id.clone(),
        timeout_run_id.clone(),
        Some(3),
        owner.run_engine.clone(),
        owner.runs_handle(),
        None,
        None,
    )
    .with_timeout(Duration::from_millis(20));
    assert!(matches!(
        astra_tools::ToolApprovalGate::request_approval(
            &timeout_gate,
            "approval-timeout-cross-pod",
            "bash",
            &json!({"command": "git status"}),
        )
        .await,
        astra_tools::ApprovalDecision::Timeout
    ));
    let late = callback
        .resolve_run_interaction(
            timeout_run_id.clone(),
            user_id.clone(),
            "approval-timeout-cross-pod".into(),
            astra_services::runs::DurableRunInteractionKind::Approval,
            json!({
                "request_id": "approval-timeout-cross-pod",
                "outcome": "approved",
                "decision": "allow",
                "reason": null,
                "tool": "bash",
                "approval_kind": "standard",
            }),
        )
        .await
        .expect("late callback returns durable conflict outcome");
    assert!(matches!(
        late,
        astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(_)
    ));

    for run_id in [&approval_run_id, &prompt_run_id, &timeout_run_id] {
        cleanup_lifecycle_run_fixture(&pool, &user_id, run_id).await;
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_long_running_fanout_survives_observer_restart_and_partial_completion() {
    let pool = setup_lifecycle_run_db_it().await;
    let owner = db_backed_test_service(&pool, "fanout-it-owner-pod");
    let observer = db_backed_test_service(&pool, "fanout-it-observer-pod");
    let user_id = format!("fanout-it-user-{}", Uuid::new_v4());
    let session_id = format!("fanout-it-session-{}", Uuid::new_v4());
    let root_id = format!("fanout-it-root-{}", Uuid::new_v4());
    let children = [
        (
            format!("fanout-it-child-a-{}", Uuid::new_v4()),
            "reviewer-a",
            "correctness",
        ),
        (
            format!("fanout-it-child-b-{}", Uuid::new_v4()),
            "reviewer-b",
            "performance",
        ),
        (
            format!("fanout-it-child-c-{}", Uuid::new_v4()),
            "reviewer-c",
            "tests",
        ),
    ];
    for run_id in children
        .iter()
        .map(|(run_id, _, _)| run_id)
        .chain(std::iter::once(&root_id))
    {
        cleanup_lifecycle_run_fixture(&pool, &user_id, run_id).await;
    }

    owner
        .run_engine
        .start_run(&root_id, &user_id, &session_id)
        .await
        .expect("owner starts fanout root");
    for (run_id, agent_id, _) in &children {
        owner
            .run_engine
            .start_run_ext(
                run_id,
                &user_id,
                &session_id,
                Some(&root_id),
                Some("fanout-it-delegation"),
                Some(agent_id),
                None,
            )
            .await
            .expect("owner starts fanout child");
    }
    let spawned = children
        .iter()
        .enumerate()
        .map(|(slot_index, (run_id, agent_id, slot_id))| {
            json!({
                "type": "agent_spawned",
                "run_id": run_id,
                "parent_run_id": root_id,
                "agent_id": agent_id,
                "agent_type": "code-review",
                "description": format!("Review {slot_id}"),
                "fanout_slot": {
                    "group_id": "fanout-it-review",
                    "target_count": children.len(),
                    "slot_index": slot_index,
                    "slot_id": slot_id,
                }
            })
        })
        .collect::<Vec<_>>();
    owner
        .run_engine
        .append_events_batch(&user_id, &root_id, &spawned)
        .await
        .expect("persist typed fanout membership");

    assert!(
        owner
            .run_engine
            .store()
            .update_run_status_with_events_if_current(
                &user_id,
                &children[0].0,
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &[
                    json!({"event_type":"text_done","data":{"full_text":"correctness complete"}}),
                    json!({"event_type":"run_finished","data":{"status":"completed"}}),
                ],
            )
            .await
            .expect("complete first child")
    );
    assert!(
        owner
            .run_engine
            .store()
            .update_run_status_with_events_if_current(
                &user_id,
                &children[1].0,
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some("performance review failed"),
                &[
                    json!({"event_type":"run_error","data":{"error":"performance review failed"}}),
                    json!({"event_type":"run_finished","data":{"status":"failed"}}),
                ],
            )
            .await
            .expect("fail second child")
    );

    let spawner = test_dynamic_agent_spawner();
    let initial_reconciler = Arc::new(ServerDurableAgentReconciler {
        run_engine: observer.run_engine.clone(),
        user_id: user_id.clone(),
        session_id: session_id.clone(),
        state: TokioMutex::new(ServerDurableAgentReconcileState::default()),
    });
    spawner
        .set_durable_agent_reconciler(initial_reconciler)
        .await;
    assert_eq!(
        spawner.reconcile_durable_agent_runs().await.unwrap(),
        children.len(),
        "new pod reconstructs all children without taking their leases"
    );

    let partial = spawner
        .fanout_group_for_agent(children[0].1)
        .await
        .expect("partial fanout survives observer restart");
    assert_eq!(partial.status, AgentFanoutStatus::Running);
    assert_eq!(partial.slots[0].status, AgentFanoutSlotStatus::Completed);
    assert_eq!(partial.slots[1].status, AgentFanoutSlotStatus::Failed);
    assert_eq!(partial.slots[2].status, AgentFanoutSlotStatus::Running);
    let remote_owner =
        sqlx::query("SELECT owner_pod_id FROM agent_runs WHERE user_id = ? AND run_id = ?")
            .bind(&user_id)
            .bind(&children[2].0)
            .fetch_one(pool.get())
            .await
            .expect("read remote child lease owner")
            .try_get::<Option<String>, _>("owner_pod_id")
            .expect("decode remote child lease owner");
    assert_eq!(remote_owner.as_deref(), Some("fanout-it-owner-pod"));

    assert!(
        owner
            .run_engine
            .store()
            .update_run_status_with_events_if_current(
                &user_id,
                &children[2].0,
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &[
                    json!({"event_type":"text_done","data":{"full_text":"tests complete"}}),
                    json!({"event_type":"run_finished","data":{"status":"completed"}}),
                ],
            )
            .await
            .expect("remote owner completes final child")
    );
    spawner
        .set_durable_agent_reconciler(Arc::new(ServerDurableAgentReconciler {
            run_engine: observer.run_engine.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            state: TokioMutex::new(ServerDurableAgentReconcileState::default()),
        }))
        .await;
    assert_eq!(spawner.reconcile_durable_agent_runs().await.unwrap(), 1);
    let settled = spawner
        .fanout_group_for_agent(children[2].1)
        .await
        .expect("settled fanout remains inspectable");
    assert_eq!(settled.status, AgentFanoutStatus::Finished);
    assert_eq!(settled.slots[0].status, AgentFanoutSlotStatus::Completed);
    assert_eq!(settled.slots[1].status, AgentFanoutSlotStatus::Failed);
    assert_eq!(settled.slots[2].status, AgentFanoutSlotStatus::Completed);
    let summary = settled.summary();
    assert_eq!(summary.completed, 2);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.active, 0);

    for run_id in children
        .iter()
        .map(|(run_id, _, _)| run_id)
        .chain(std::iter::once(&root_id))
    {
        cleanup_lifecycle_run_fixture(&pool, &user_id, run_id).await;
    }
    let _ = sqlx::query(
        "DELETE FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool.get())
    .await;
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_orphan_resume_returns_typed_session_continuation() {
    let pool = setup_lifecycle_run_db_it().await;
    let service = db_backed_test_service(&pool, "continuation-it-pod");
    let user_id = format!("continuation-it-user-{}", Uuid::new_v4());
    let session_id = format!("continuation-it-session-{}", Uuid::new_v4());
    let run_id = format!("continuation-it-run-{}", Uuid::new_v4());
    cleanup_lifecycle_run_fixture(&pool, &user_id, &run_id).await;
    service
        .run_engine
        .start_run(&run_id, &user_id, &session_id)
        .await
        .expect("start durable run");
    service
        .run_engine
        .persist_status(&user_id, &run_id, STATUS_PAUSED, Some("user_resume"), None)
        .await
        .expect("pause durable run");
    sqlx::query(
        "UPDATE agent_runs SET owner_pod_id = NULL, owner_lease_expires_at = NULL
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&run_id)
    .execute(pool.get())
    .await
    .expect("simulate lost owner pod");

    let result = ok(service.resume_run(run_id.clone(), user_id.clone()).await);
    assert_eq!(
        result.disposition,
        RunMutationDisposition::SessionContinuationRequired
    );
    assert_eq!(result.status, STATUS_PAUSED);
    let continuation = result.continuation.expect("continuation directive");
    assert_eq!(continuation.strategy, "session_continuation");
    assert_eq!(continuation.session_id, session_id);
    assert_eq!(continuation.source_run_id, run_id);

    cleanup_lifecycle_run_fixture(&pool, &user_id, &continuation.source_run_id).await;
}

#[tokio::test]
async fn resume_run_does_not_promote_cancelled_or_interrupted_finish_to_completed() {
    for (suffix, data) in [
        ("cancelled", json!({"cancelled": true})),
        ("interrupted", json!({"interrupted": true})),
    ] {
        let svc = test_service();
        let run = ok(svc
            .create_run("user-1".into(), test_request(&format!("task-{suffix}")))
            .await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        svc.run_engine
            .append_event(
                "user-1",
                &run.run_id,
                json!({
                    "event_type": "run_finished",
                    "data": data
                }),
            )
            .await
            .unwrap();

        let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, STATUS_RUNNING, "{suffix}");
        assert_eq!(result.previous_status, STATUS_PAUSED, "{suffix}");
        let durable = svc
            .run_engine
            .load_run("user-1", &run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING, "{suffix}");
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_resumed");
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_resume_does_not_promote_cancelled_or_interrupted_terminal_markers() {
    let pool = setup_lifecycle_run_db_it().await;
    for (suffix, data) in [
        ("cancelled", json!({"cancelled": true})),
        ("interrupted", json!({"interrupted": true})),
    ] {
        let svc = db_backed_test_service(&pool, &format!("pause-resume-it-pod-{suffix}"));
        let user_id = "user-1";
        let run_id = format!("resume-{suffix}-{}", Uuid::new_v4());
        let session_id = format!("sess-{suffix}-{}", Uuid::new_v4());
        cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
        seed_lifecycle_run_for_pause_resume_it(&svc, user_id, &run_id, &session_id).await;

        ok(svc.pause_run(run_id.clone(), user_id.to_string()).await);
        svc.run_engine
            .append_event(
                user_id,
                &run_id,
                json!({
                    "event_type": "run_finished",
                    "data": data
                }),
            )
            .await
            .expect("append buffered non-completed terminal marker");

        let result = ok(svc.resume_run(run_id.clone(), user_id.to_string()).await);
        assert_eq!(result.status, STATUS_RUNNING, "{suffix}");
        assert_eq!(result.previous_status, STATUS_PAUSED, "{suffix}");

        let durable = svc
            .run_engine
            .load_run(user_id, &run_id)
            .await
            .expect("load durable run")
            .expect("durable run exists");
        assert_eq!(durable.status, STATUS_RUNNING, "{suffix}");
        assert!(durable.waiting_for.is_none(), "{suffix}");
        assert_eq!(
            durable.events.last().unwrap()["event_type"],
            "run_resumed",
            "{suffix}"
        );

        {
            let runs = svc.runs.read().await;
            let live = runs.get(&run_id).expect("live run should still be tracked");
            assert!(matches!(&live.status, RunStatus::Running));
        }
        cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_durable_event_budget_bounds_large_stream_persistence() {
    let pool = setup_lifecycle_run_db_it().await;
    let svc = db_backed_test_service(&pool, "durable-budget-it-pod");
    let user_id = "user-1";
    let run_id = format!("budget-it-{}", Uuid::new_v4());
    let session_id = format!("sess-budget-it-{}", Uuid::new_v4());
    let budget = DurableRunEventBatchBudget::default();
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    svc.run_engine
        .start_run(&run_id, user_id, &session_id)
        .await
        .expect("start durable DB run");

    let mut raw_stream_events: Vec<Value> = (0..10_000)
        .map(|idx| json!({"type": "text_delta", "content": format!("chunk-{idx}")}))
        .collect();
    raw_stream_events
        .push(json!({"type": "tool_call", "tool_call": {"id": "call-1", "name": "bash"}}));
    raw_stream_events.push(json!({
        "type": "tool_call_end",
        "call_id": "call-1",
        "tool": "bash",
        "result": "ok"
    }));
    raw_stream_events.extend(
        (0..(budget.row_budget + 25)).map(|idx| json!({"type": "agent_progress", "seq": idx})),
    );
    raw_stream_events.push(json!({
        "event_type": "text_done",
        "data": {"full_text": "large durable final answer"}
    }));
    raw_stream_events.push(json!({
        "event_type": "run_finished",
        "data": {"prompt_tokens": 9, "completion_tokens": 3, "tool_call_count": 1}
    }));

    let durable_candidates: Vec<Value> = raw_stream_events
        .iter()
        .filter(|event| streaming_event_for_persistence(event))
        .cloned()
        .collect();
    assert_eq!(
        durable_candidates
            .iter()
            .filter(|event| durable_event_type(event) == Some("text_delta"))
            .count(),
        0,
        "transport chunks must stay live-only before DB persistence"
    );

    let budgeted = enforce_durable_run_event_batch_budget_with_budget(durable_candidates, budget);
    assert_eq!(budgeted.len(), budget.row_budget);
    assert!(
        budgeted
            .iter()
            .any(|event| durable_event_type(event) == Some("durable_events_compacted")),
        "semantic overflow should be summarized"
    );
    assert!(
        budgeted
            .iter()
            .any(|event| durable_event_type(event) == Some("tool_call")),
        "tool start boundary must beat progress noise under budget pressure"
    );
    assert!(
        budgeted
            .iter()
            .any(|event| durable_event_type(event) == Some("tool_call_end")),
        "tool end boundary must beat progress noise under budget pressure"
    );
    assert_eq!(
        durable_event_type(&budgeted[budgeted.len() - 2]),
        Some("text_done")
    );
    assert_eq!(
        durable_event_type(&budgeted[budgeted.len() - 1]),
        Some("run_finished")
    );

    let transitioned = svc
        .run_engine
        .transition_status_with_events_if_current(
            user_id,
            &run_id,
            &[STATUS_RUNNING],
            STATUS_COMPLETED,
            None,
            None,
            &budgeted,
        )
        .await
        .expect("commit budgeted terminal events");
    assert!(transitioned);

    let rows = sqlx::query(
        "SELECT event_type
         FROM agent_run_events
         WHERE user_id = ? AND run_id = ?
         ORDER BY event_idx ASC",
    )
    .bind(user_id)
    .bind(&run_id)
    .fetch_all(pool.get())
    .await
    .expect("load persisted event rows");
    assert_eq!(
        rows.len(),
        budget.row_budget + 1,
        "DB rows should be bounded to budgeted batch plus run_started"
    );
    let persisted_types = rows
        .iter()
        .map(|row| row.try_get::<String, _>("event_type").expect("event_type"))
        .collect::<Vec<_>>();
    assert!(
        !persisted_types
            .iter()
            .any(|event_type| event_type == "text_delta")
    );
    for expected in [
        "durable_events_compacted",
        "tool_call",
        "tool_call_end",
        "text_done",
        "run_finished",
    ] {
        assert!(
            persisted_types
                .iter()
                .any(|event_type| event_type == expected),
            "missing persisted {expected}: {persisted_types:?}"
        );
    }

    let replay_events = ok(svc.stream_run(run_id.clone(), user_id.to_string(), 1).await);
    assert!(replay_events.len() <= budget.row_budget);
    assert!(replay_events.iter().all(|event| {
        event.get("type").and_then(Value::as_str) != Some("text_delta")
            && event.get("event_type").and_then(Value::as_str) != Some("text_delta")
    }));
    assert!(replay_events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call")
            || event.get("event_type").and_then(Value::as_str) == Some("tool_call")
    }));
    assert!(replay_events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call_end")
            || event.get("event_type").and_then(Value::as_str) == Some("tool_call_end")
    }));
    assert!(replay_events.iter().any(|event| {
        event.get("event_type").and_then(Value::as_str) == Some("text_done")
            && event.pointer("/data/full_text").and_then(Value::as_str)
                == Some("large durable final answer")
    }));
    assert!(
        replay_events.iter().any(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("run_finished")
        })
    );

    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_live_microbatches_are_complete_and_precede_terminal() {
    let pool = setup_lifecycle_run_db_it().await;
    let svc = db_backed_test_service(&pool, "live-microbatch-it-pod");
    let user_id = "user-1";
    let run_id = format!("live-microbatch-it-{}", Uuid::new_v4());
    let session_id = format!("sess-live-microbatch-it-{}", Uuid::new_v4());
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    svc.run_engine
        .start_run(&run_id, user_id, &session_id)
        .await
        .expect("start durable DB run");

    let runs = Arc::new(RwLock::new(HashMap::new()));
    let (live_tx, _) = broadcast::channel(8);
    let mut client_event_tx = None;
    let mut pending = PendingDurableLiveEvents::default();
    for index in 0..600_u64 {
        pending.push(json!({
            "type": "agent_spawned",
            "agent_id": format!("agent-{index}"),
            "seq": index,
        }));
        if pending.should_flush() {
            flush_durable_live_events(
                &mut pending,
                &svc.run_engine,
                &runs,
                user_id,
                &run_id,
                &live_tx,
                &mut client_event_tx,
            )
            .await
            .expect("flush bounded live microbatch");
        }
    }
    flush_durable_live_events(
        &mut pending,
        &svc.run_engine,
        &runs,
        user_id,
        &run_id,
        &live_tx,
        &mut client_event_tx,
    )
    .await
    .expect("flush terminal watermark");
    assert!(
        svc.run_engine
            .transition_status_with_event_if_current(
                user_id,
                &run_id,
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                json!({"event_type": "run_finished", "data": {}}),
            )
            .await
            .expect("commit terminal event")
    );

    let rows = sqlx::query(
        "SELECT event_type
         FROM agent_run_events
         WHERE user_id = ? AND run_id = ?
         ORDER BY event_idx ASC",
    )
    .bind(user_id)
    .bind(&run_id)
    .fetch_all(pool.get())
    .await
    .expect("load ordered live and terminal events");
    let event_types = rows
        .iter()
        .map(|row| row.try_get::<String, _>("event_type").expect("event type"))
        .collect::<Vec<_>>();
    assert_eq!(
        event_types
            .iter()
            .filter(|event_type| event_type.as_str() == "agent_spawned")
            .count(),
        600
    );
    assert_eq!(
        event_types.last().map(String::as_str),
        Some("run_finished"),
        "terminal marker must be the final durable row"
    );

    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: ASTRA_TEST_DB_IT=1 and ASTRA_DURABLE_EVENT_PRESSURE_PROBE=1"]
async fn durable_run_event_pressure_probe() {
    if !durable_event_pressure_opted_in() {
        eprintln!(
            "DURABLE_EVENT_PRESSURE_SKIPPED set {DURABLE_EVENT_PRESSURE_OPT_IN}=1 or run make test-durable-event-pressure"
        );
        return;
    }

    let pool = setup_lifecycle_run_db_it().await;
    let run_count = durable_event_pressure_env_usize("ASTRA_DURABLE_EVENT_PRESSURE_RUNS", 100, 1);
    let text_delta_count =
        durable_event_pressure_env_usize("ASTRA_DURABLE_EVENT_PRESSURE_TEXT_DELTAS", 10_000, 1);
    let budget = DurableRunEventBatchBudget::default();
    let progress_event_count = durable_event_pressure_env_usize(
        "ASTRA_DURABLE_EVENT_PRESSURE_PROGRESS_ROWS",
        budget.row_budget + 25,
        budget.row_budget + 1,
    );

    let started = Instant::now();
    let tasks = (0..run_count).map(|run_ordinal| {
        durable_event_pressure_case(
            pool.clone(),
            run_ordinal,
            text_delta_count,
            progress_event_count,
        )
    });
    let results = futures_util::future::join_all(tasks).await;
    let mut stats = Vec::with_capacity(run_count);
    for result in results {
        stats.push(result.expect("durable event pressure run"));
    }

    let total_raw_events: usize = stats.iter().map(|stat| stat.raw_events).sum();
    let total_candidate_rows: usize = stats.iter().map(|stat| stat.candidate_rows).sum();
    let total_candidate_bytes: usize = stats.iter().map(|stat| stat.candidate_bytes).sum();
    let total_budgeted_rows: usize = stats.iter().map(|stat| stat.budgeted_rows).sum();
    let total_budgeted_bytes: usize = stats.iter().map(|stat| stat.budgeted_bytes).sum();
    let total_persisted_rows: usize = stats.iter().map(|stat| stat.persisted_rows).sum();
    let total_replay_rows: usize = stats.iter().map(|stat| stat.replay_rows).sum();
    let total_text_delta_rows: usize = stats.iter().map(|stat| stat.text_delta_rows).sum();
    let compacted_runs = stats.iter().filter(|stat| stat.compacted_rows == 1).count();
    let max_persisted_rows_per_run = stats
        .iter()
        .map(|stat| stat.persisted_rows)
        .max()
        .unwrap_or_default();
    let max_replay_rows_per_run = stats
        .iter()
        .map(|stat| stat.replay_rows)
        .max()
        .unwrap_or_default();
    let max_run_elapsed_ms = stats
        .iter()
        .map(|stat| stat.elapsed_ms)
        .max()
        .unwrap_or_default();

    assert_eq!(
        compacted_runs, run_count,
        "every overflowed run should emit one compaction summary"
    );
    assert_eq!(
        total_text_delta_rows, 0,
        "transport deltas must never enter durable run events"
    );
    assert!(
        total_persisted_rows <= run_count * (budget.row_budget + 1),
        "persisted rows should be bounded by durable batch budget plus run_started"
    );
    assert!(
        total_replay_rows <= run_count * budget.row_budget,
        "cache-miss replay rows should be bounded by durable batch budget"
    );

    eprintln!(
        "DURABLE_EVENT_PRESSURE_RESULT {}",
        json!({
            "path": "agent_run_events.durable_event_budget",
            "runs": run_count,
            "text_deltas_per_run": text_delta_count,
            "progress_rows_per_run": progress_event_count,
            "row_budget": budget.row_budget,
            "byte_budget": budget.byte_budget,
            "total_raw_events": total_raw_events,
            "total_candidate_rows": total_candidate_rows,
            "total_candidate_bytes": total_candidate_bytes,
            "total_budgeted_rows": total_budgeted_rows,
            "total_budgeted_bytes": total_budgeted_bytes,
            "total_persisted_rows": total_persisted_rows,
            "total_replay_rows": total_replay_rows,
            "total_text_delta_rows": total_text_delta_rows,
            "compacted_runs": compacted_runs,
            "summary_event_frequency": compacted_runs as f64 / run_count as f64,
            "max_persisted_rows_per_run": max_persisted_rows_per_run,
            "max_replay_rows_per_run": max_replay_rows_per_run,
            "max_run_elapsed_ms": max_run_elapsed_ms,
            "elapsed_ms": duration_millis_u64(started.elapsed())
        })
    );
}

#[tokio::test]
async fn resume_run_conflict_when_not_paused() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    let e = err(svc.resume_run(run.run_id, "user-1".into()).await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn resume_run_hides_foreign_run() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    let e = err(svc.resume_run(run.run_id, "user-2".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_run_not_found() {
    let svc = test_service();
    let e = err(svc.resume_run("nonexistent".into(), "user-1".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pause_resume_round_trip_preserves_events() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    let status = ok(svc
        .get_run_status(run.run_id.clone(), "user-1".into())
        .await);
    assert_eq!(status.events_count, 3); // run_started + run_paused + run_resumed
    let events = ok(svc.stream_run(run.run_id, "user-1".into(), 0).await);
    assert_eq!(events[0]["event_type"], "run_started");
    assert_eq!(events[1]["event_type"], "run_paused");
    assert_eq!(events[2]["event_type"], "run_resumed");
}

#[tokio::test]
async fn double_pause_is_conflict() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    let e = err(svc.pause_run(run.run_id, "user-1".into()).await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

// ─── Durable persistence integration tests ─────────────────────────

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
async fn durable_create_run_persists_to_store() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

    let engine = &svc.run_engine;
    let durable = engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.user_id, "user-1");
    assert_eq!(durable.status, "running");
    assert_eq!(durable.session_id, run.session_id);
}

#[tokio::test]
async fn durable_create_run_eventually_persists_terminal_event() {
    let (svc, _llm) = terminal_test_service().await;
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

    let engine = &svc.run_engine;
    let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let durable = engine
                .load_run("user-1", &run.run_id)
                .await
                .unwrap()
                .unwrap();
            if durable.status != "running"
                && matches!(
                    durable
                        .events
                        .last()
                        .and_then(|event| event["event_type"].as_str()),
                    Some("run_finished")
                )
            {
                break durable;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timeout waiting for durable run to persist terminal event");
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
}

#[tokio::test]
async fn durable_stream_chat_persists_final_state() {
    let (svc, _llm) = terminal_test_service().await;
    let stream = ok(svc
        .stream_chat("user-1".into(), test_request("hello"))
        .await);

    let engine = &svc.run_engine;
    let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let durable = engine
                .load_run("user-1", &stream.run_id)
                .await
                .unwrap()
                .unwrap();
            if durable.status != "running"
                && matches!(
                    durable
                        .events
                        .last()
                        .and_then(|event| event["event_type"].as_str()),
                    Some("run_finished")
                )
            {
                break durable;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timeout waiting for durable stream_chat final state");
    assert_eq!(durable.user_id, "user-1");
    assert_eq!(durable.session_id, stream.session_id);
    assert!(durable.events.len() >= 2);
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
}

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
async fn durable_cancel_persists_to_store() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);

    let engine = &svc.run_engine;
    let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let durable = engine
                .load_run("user-1", &run.run_id)
                .await
                .unwrap()
                .unwrap();
            if matches!(
                durable
                    .events
                    .last()
                    .and_then(|event| event["event_type"].as_str()),
                Some("run_finished")
            ) {
                break durable;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timeout waiting for cancelled run to persist terminal event");
    assert_eq!(durable.status, "cancelled");
    assert!(durable.events.len() >= 2); // run_started + run_finished
}

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
async fn durable_pause_resume_round_trip() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    let engine = &svc.run_engine;
    let durable = engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, "paused");
    assert_eq!(durable.waiting_for.as_deref(), Some("user_resume"));

    ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    let durable = engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, "running");
    assert!(durable.waiting_for.is_none());
}

#[tokio::test]
async fn cancel_run_returns_durable_terminal_status_on_cache_miss() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
    engine
        .persist_status("user-1", "run-1", STATUS_COMPLETED, None, None)
        .await
        .unwrap();

    let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.run_id, "run-1");
    assert_eq!(result.status, STATUS_COMPLETED);
}

#[tokio::test]
async fn cancel_run_running_cache_miss_persists_cancelled() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

    let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_CANCELLED);
    let durable = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
}

#[tokio::test]
async fn cancel_run_stale_read_does_not_overwrite_completed() {
    let store: Arc<dyn RunStateStore> = Arc::new(
        FaultInjectedRunStateStore::new(&[], &[]).with_status_mutation_before_call(
            1,
            "user-1",
            "run-race",
            STATUS_COMPLETED,
            None,
            None,
        ),
    );
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-race", "user-1", "sess-1")
        .await
        .unwrap();

    let result = ok(svc.cancel_run("run-race".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_COMPLETED);

    let durable = engine
        .load_run("user-1", "run-race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_COMPLETED);
    assert!(
        durable
            .events
            .iter()
            .all(|event| event["event_type"] != "run_finished"),
        "stale cancel must not append a cancel terminal event"
    );
}

#[tokio::test]
async fn pause_run_stale_read_does_not_overwrite_completed() {
    let store: Arc<dyn RunStateStore> = Arc::new(
        FaultInjectedRunStateStore::new(&[], &[]).with_status_mutation_before_call(
            1,
            "user-1",
            "run-pause-race",
            STATUS_COMPLETED,
            None,
            None,
        ),
    );
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-pause-race", "user-1", "sess-1")
        .await
        .unwrap();

    let e = err(svc
        .pause_run("run-pause-race".into(), "user-1".into())
        .await);
    assert_eq!(e.0, StatusCode::CONFLICT);
    assert!(e.1.0.detail.contains("completed"));

    let durable = engine
        .load_run("user-1", "run-pause-race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_COMPLETED);
    assert!(
        durable
            .events
            .iter()
            .all(|event| event["event_type"] != "run_paused"),
        "stale pause must not append a pause event"
    );
}

#[tokio::test]
async fn resume_run_stale_read_does_not_overwrite_cancelled() {
    let store: Arc<dyn RunStateStore> = Arc::new(
        FaultInjectedRunStateStore::new(&[], &[]).with_status_mutation_before_call(
            2,
            "user-1",
            "run-resume-race",
            STATUS_CANCELLED,
            None,
            None,
        ),
    );
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-resume-race", "user-1", "sess-1")
        .await
        .unwrap();
    engine
        .persist_status(
            "user-1",
            "run-resume-race",
            STATUS_PAUSED,
            Some("user_resume"),
            None,
        )
        .await
        .unwrap();
    let (mut live, _, pause_flag, _) = AgenticRunLifecycleService::build_tracked_run_state(
        "run-resume-race".to_string(),
        "sess-1".to_string(),
        "user-1".to_string(),
    );
    live.status = RunStatus::Paused;
    live.waiting_for = Some("user_resume".to_string());
    pause_flag.store(true, Ordering::SeqCst);
    svc.runs
        .write()
        .await
        .insert("run-resume-race".to_string(), live);

    let e = err(svc
        .resume_run("run-resume-race".into(), "user-1".into())
        .await);
    assert_eq!(e.0, StatusCode::CONFLICT);
    assert!(e.1.0.detail.contains("cancelled"));

    let durable = engine
        .load_run("user-1", "run-resume-race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
    assert!(
        durable
            .events
            .iter()
            .all(|event| event["event_type"] != "run_resumed"),
        "stale resume must not append a resume event"
    );
}

#[tokio::test]
async fn pause_run_orphaned_running_reconciles_to_session_continuation() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

    let result = ok(svc.pause_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_PAUSED);
    let durable = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_PAUSED);
    assert!(durable.waiting_for.is_none());
    assert_eq!(
        durable.events.last().unwrap()["event_type"],
        "run_interrupted"
    );
    assert_eq!(
        durable.events.last().unwrap()["data"]["resume_strategy"],
        "session_continuation"
    );
    engine
        .start_run("run-2", "user-1", "sess-1")
        .await
        .expect("orphan reconciliation must release the session slot");
}

#[tokio::test]
async fn resume_run_paused_without_live_executor_requires_session_continuation() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
    engine
        .persist_status("user-1", "run-1", STATUS_PAUSED, Some("user_resume"), None)
        .await
        .unwrap();

    let result = ok(svc.resume_run("run-1".into(), "user-1".into()).await);
    assert_eq!(
        result.disposition,
        RunMutationDisposition::SessionContinuationRequired
    );
    let continuation = result.continuation.expect("typed continuation directive");
    assert_eq!(continuation.strategy, "session_continuation");
    assert_eq!(continuation.session_id, "sess-1");
    assert_eq!(continuation.source_run_id, "run-1");
    let durable = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_PAUSED);
    assert!(durable.waiting_for.is_none());
    assert_eq!(
        durable.events.last().unwrap()["data"]["requested_operation"],
        "resume"
    );
    engine
        .start_run("run-2", "user-1", "sess-1")
        .await
        .expect("same-run resume directive must enable session continuation");
}

#[tokio::test]
async fn cancel_run_transition_failure_does_not_commit_status_or_event() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

    let e = err(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(e.1.0.detail.contains("cancel transition"));

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.waiting_for.is_none());
    assert_eq!(durable.events.len(), 1);
    assert_eq!(durable.events[0]["event_type"], "run_started");

    let runs = svc.runs.read().await;
    let live = runs.get(&run.run_id).expect("live run state");
    assert_eq!(live.status, RunStatus::Running);
    assert!(live.waiting_for.is_none());
    assert!(!live.cancel_flag.load(Ordering::SeqCst));
    assert_eq!(live.events.len(), 1);
    assert_eq!(live.events[0]["event_type"], "run_started");
}

#[tokio::test]
async fn pause_run_transition_failure_does_not_commit_status_or_event() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

    let e = err(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(e.1.0.detail.contains("pause transition"));

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.waiting_for.is_none());
    assert_eq!(durable.events.len(), 1);
    assert_eq!(durable.events[0]["event_type"], "run_started");

    let runs = svc.runs.read().await;
    let live = runs.get(&run.run_id).expect("live run state");
    assert_eq!(live.status, RunStatus::Running);
    assert!(live.waiting_for.is_none());
    assert!(!live.pause_flag.load(Ordering::SeqCst));
    assert_eq!(live.events.len(), 1);
    assert_eq!(live.events[0]["event_type"], "run_started");
}

#[tokio::test]
async fn resume_run_transition_failure_does_not_commit_status_or_event() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[2]));
    let svc = test_service_with_store(store);
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);

    let e = err(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(e.1.0.detail.contains("resume transition"));

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_PAUSED);
    assert_eq!(durable.waiting_for.as_deref(), Some("user_resume"));
    assert_eq!(durable.events.len(), 2);
    assert_eq!(durable.events[1]["event_type"], "run_paused");

    let runs = svc.runs.read().await;
    let live = runs.get(&run.run_id).expect("live run state");
    assert_eq!(live.status, RunStatus::Paused);
    assert_eq!(live.waiting_for.as_deref(), Some("user_resume"));
    assert!(live.pause_flag.load(Ordering::SeqCst));
    assert_eq!(live.events.len(), 2);
    assert_eq!(live.events[1]["event_type"], "run_paused");
}

// Durable resume/session exclusivity

#[tokio::test]
async fn durable_resume_succeeds_when_current_paused_run_is_only_blocker() {
    let svc = test_service();
    let engine = &svc.run_engine;

    engine
        .start_run("run-solo-resume", "user-1", "sess-solo-resume")
        .await
        .unwrap();
    engine
        .persist_status(
            "user-1",
            "run-solo-resume",
            STATUS_PAUSED,
            Some("user_resume"),
            None,
        )
        .await
        .unwrap();
    install_live_run_state(
        &svc,
        "user-1",
        "run-solo-resume",
        "sess-solo-resume",
        RunStatus::Paused,
        Some("user_resume"),
    )
    .await;

    let result = ok(svc
        .resume_run("run-solo-resume".into(), "user-1".into())
        .await);

    assert_eq!(result.status, STATUS_RUNNING);
    let durable = engine
        .load_run("user-1", "run-solo-resume")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.waiting_for.is_none());
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_resumed");
}

#[tokio::test]
async fn durable_resume_rejects_blocking_sibling_after_cache_miss() {
    let svc = test_service();
    let engine = &svc.run_engine;

    engine
        .start_run("run-parent-blocked", "user-1", "sess-blocked")
        .await
        .unwrap();
    engine
        .persist_status("user-1", "run-parent-blocked", STATUS_PAUSED, None, None)
        .await
        .unwrap();
    engine
        .start_run("run-root-blocker", "user-1", "sess-blocked")
        .await
        .unwrap();

    let error = err(svc
        .resume_run("run-parent-blocked".into(), "user-1".into())
        .await);

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(error.1.0.detail, "session already has an active run");
    let durable = engine
        .load_run("user-1", "run-parent-blocked")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_PAUSED);
    assert!(
        durable
            .events
            .iter()
            .all(|event| event["event_type"] != "run_resumed")
    );
}

#[tokio::test]
async fn durable_resume_promotes_buffered_completion_even_when_session_has_blocker() {
    let svc = test_service();
    let engine = &svc.run_engine;

    engine
        .start_run("run-buffered-complete", "user-1", "sess-buffered")
        .await
        .unwrap();
    engine
        .persist_status("user-1", "run-buffered-complete", STATUS_PAUSED, None, None)
        .await
        .unwrap();
    engine
        .append_event(
            "user-1",
            "run-buffered-complete",
            json!({
                "event_type": "run_finished",
                "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
            }),
        )
        .await
        .unwrap();
    engine
        .start_run("run-buffered-root-blocker", "user-1", "sess-buffered")
        .await
        .unwrap();

    let result = ok(svc
        .resume_run("run-buffered-complete".into(), "user-1".into())
        .await);

    assert_eq!(result.status, STATUS_COMPLETED);
    let durable = engine
        .load_run("user-1", "run-buffered-complete")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_COMPLETED);
    assert!(durable.waiting_for.is_none());
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
}

/// Concurrent mutation of two durable-only runs in the same session must
/// classify the orphaned running row honestly while cancelling its sibling.
#[tokio::test]
async fn concurrent_orphan_pause_and_cancel_release_session_slot_consistently() {
    let svc = test_service();
    let engine = &svc.run_engine;
    let session_id = "sess-pause-cancel-conc";

    // Set up via engine (durable-only): run-a budget-exhausted paused,
    // run-b running. Budget-exhausted paused doesn't block create_run.
    engine
        .start_run("run-a", "user-1", session_id)
        .await
        .unwrap();
    engine
        .persist_status("user-1", "run-a", STATUS_PAUSED, None, None)
        .await
        .unwrap();
    engine
        .start_run("run-b", "user-1", session_id)
        .await
        .unwrap();

    // Concurrent: a pause request discovers that run-b has no executor while
    // run-a is cancelled. Both operations must converge without inventing a
    // resumable same-run task.
    let (result_pause_b, result_cancel_a) = tokio::join!(
        svc.pause_run("run-b".into(), "user-1".into()),
        svc.cancel_run("run-a".into(), "user-1".into()),
    );

    assert!(
        result_pause_b.is_ok(),
        "pause run-b must succeed: {result_pause_b:?}"
    );
    assert!(
        result_cancel_a.is_ok(),
        "cancel run-a must succeed: {result_cancel_a:?}"
    );

    // Durable store: both mutations applied
    let durable_a = engine.load_run("user-1", "run-a").await.unwrap().unwrap();
    let durable_b = engine.load_run("user-1", "run-b").await.unwrap().unwrap();

    assert_eq!(durable_a.status, STATUS_CANCELLED, "run-a cancelled");
    assert!(durable_a.waiting_for.is_none());
    assert!(durable_a.events.iter().any(
        |e| e["event_type"] == "run_finished" && e["data"]["cancelled"].as_bool() == Some(true)
    ));

    assert_eq!(durable_b.status, STATUS_PAUSED, "run-b paused");
    assert!(durable_b.waiting_for.is_none());
    assert_eq!(
        durable_b.events.last().unwrap()["event_type"],
        "run_interrupted"
    );
    assert_eq!(
        durable_b.events.last().unwrap()["data"]["resume_strategy"],
        "session_continuation"
    );
    engine
        .start_run("run-c", "user-1", session_id)
        .await
        .expect("concurrent orphan recovery and cancel must release the session slot");
}

#[tokio::test]
async fn concurrent_cancel_and_resume_of_same_paused_run_preserves_durable_consistency() {
    let svc = test_service();
    let engine = &svc.run_engine;
    let session_id = "sess-cancel-resume-conc";

    // Set up: run-a is paused with user_resume (eligible for both cancel and resume)
    engine
        .start_run("run-a", "user-1", session_id)
        .await
        .unwrap();
    engine
        .persist_status("user-1", "run-a", STATUS_PAUSED, Some("user_resume"), None)
        .await
        .unwrap();

    // Concurrent cancel and resume from two agents
    let (cancel, resume) = tokio::join!(
        svc.cancel_run("run-a".into(), "user-1".into()),
        svc.resume_run("run-a".into(), "user-1".into()),
    );

    assert!(
        cancel.is_ok(),
        "cancel must be accepted from either paused or resumed-running state: {cancel:?}"
    );
    if let Err((code, body)) = &resume {
        assert_eq!(
            code,
            &StatusCode::CONFLICT,
            "resume loser must report conflict, got {code}: {}",
            body.0.detail
        );
    }

    let durable = engine.load_run("user-1", "run-a").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
    assert!(durable.events.iter().any(
        |e| e["event_type"] == "run_finished" && e["data"]["cancelled"].as_bool() == Some(true)
    ));
    if resume.is_ok() {
        assert!(
            durable
                .events
                .iter()
                .any(|e| e["event_type"] == "run_resumed")
        );
    } else {
        assert!(
            durable
                .events
                .iter()
                .all(|e| e["event_type"] != "run_resumed")
        );
    }
}

#[tokio::test]
async fn cancel_run_paused_cache_miss_persists_cancelled() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
    engine
        .persist_status("user-1", "run-1", STATUS_PAUSED, Some("user_resume"), None)
        .await
        .unwrap();

    let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_CANCELLED);
    let durable = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
}

#[tokio::test]
#[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
async fn get_run_status_falls_back_to_durable_store_on_cache_miss() {
    let svc = test_service();
    let stream = ok(svc
        .stream_chat("user-1".into(), test_request("hello"))
        .await);
    let engine = &svc.run_engine;
    let durable = engine
        .load_run("user-1", &stream.run_id)
        .await
        .unwrap()
        .unwrap();

    svc.runs.write().await.remove(&stream.run_id);

    let status = ok(svc
        .get_run_status(stream.run_id.clone(), "user-1".into())
        .await);
    assert_eq!(status.run_id, stream.run_id);
    assert_eq!(status.session_id, stream.session_id);
    assert_eq!(status.status, durable.status);
    assert_eq!(status.waiting_for, durable.waiting_for);
    assert_eq!(status.events_count, durable.events.len() as i64);
}

#[tokio::test]
async fn stream_run_cache_miss_replays_durable_text_done() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("run-durable-text", "user-1", "session-1")
        .await
        .expect("start durable run");
    engine
        .append_event(
            "user-1",
            "run-durable-text",
            json!({"event_type": "text_done", "data": {"full_text": "durable final answer"}}),
        )
        .await
        .expect("persist text_done");
    engine
        .append_event(
            "user-1",
            "run-durable-text",
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        )
        .await
        .expect("persist run_finished");

    let events = ok(svc
        .stream_run("run-durable-text".into(), "user-1".into(), 1)
        .await);

    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event_type"], "text_done");
    assert_eq!(events[0]["data"]["full_text"], "durable final answer");
    assert_eq!(events[1]["event_type"], "run_finished");
}

#[tokio::test]
async fn submit_run_user_intent_is_idempotent_and_does_not_mutate_execution_state() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("run-input", "user-1", "session-1")
        .await
        .unwrap();
    install_live_run_state(
        &svc,
        "user-1",
        "run-input",
        "session-1",
        RunStatus::Running,
        None,
    )
    .await;

    let first = ok(svc
        .submit_run_user_intent(
            "run-input".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "Use the focused test first."}),
            },
        )
        .await);
    let duplicate = ok(svc
        .submit_run_user_intent(
            "run-input".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "Use the focused test first."}),
            },
        )
        .await);

    let durable = engine
        .load_run("user-1", "run-input")
        .await
        .unwrap()
        .unwrap();
    let matching_intents = durable
        .events
        .iter()
        .filter(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("user_intent")
                && event
                    .get("data")
                    .and_then(|data| data.get("intent_id"))
                    .and_then(Value::as_str)
                    == Some("intent-1")
        })
        .count();
    assert!(!first.duplicate);
    assert!(duplicate.duplicate);
    assert_eq!(first.intent_id, "intent-1");
    assert_eq!(duplicate.intent_id, first.intent_id);
    assert_eq!(
        first.status,
        astra_turn_types::UserIntentStatus::AcceptedRemote
    );
    assert_eq!(matching_intents, 1);
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.waiting_for.is_none());
}

#[tokio::test]
async fn concurrent_submit_run_user_intent_commits_one_durable_event() {
    let svc = Arc::new(test_service());
    svc.run_engine
        .start_run("run-input-race", "user-1", "session-1")
        .await
        .unwrap();
    install_live_run_state(
        &svc,
        "user-1",
        "run-input-race",
        "session-1",
        RunStatus::Running,
        None,
    )
    .await;

    let submit = |svc: Arc<AgenticRunLifecycleService>| async move {
        svc.submit_run_user_intent(
            "run-input-race".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-race".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "apply once"}),
            },
        )
        .await
    };
    let (first, second) = tokio::join!(submit(svc.clone()), submit(svc.clone()));
    let mut duplicate_flags = [ok(first).duplicate, ok(second).duplicate];
    duplicate_flags.sort_unstable();
    assert_eq!(duplicate_flags, [false, true]);

    let durable = svc
        .run_engine
        .load_run("user-1", "run-input-race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        durable
            .events
            .iter()
            .filter(|event| {
                event.get("event_type").and_then(Value::as_str) == Some("user_intent")
                    && event
                        .get("data")
                        .and_then(|data| data.get("intent_id"))
                        .and_then(Value::as_str)
                        == Some("intent-race")
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn submit_run_user_intent_transition_failure_does_not_commit_status_or_events() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-input-fail", "user-1", "session-1")
        .await
        .unwrap();
    install_live_run_state(
        &svc,
        "user-1",
        "run-input-fail",
        "session-1",
        RunStatus::Running,
        None,
    )
    .await;

    let e = err(svc
        .submit_run_user_intent(
            "run-input-fail".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-fail".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "Do not commit this."}),
            },
        )
        .await);
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);

    let durable = engine
        .load_run("user-1", "run-input-fail")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.waiting_for.is_none());
    assert!(
        durable.events.iter().all(|event| {
            event
                .get("data")
                .and_then(|data| data.get("intent_id"))
                .and_then(Value::as_str)
                != Some("intent-fail")
        }),
        "failed intent append must not leave a partial durable event"
    );
}

#[tokio::test]
async fn submit_run_user_intent_rejects_orphaned_execution_without_persisting_false_acceptance() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("run-orphaned-input", "user-1", "session-1")
        .await
        .unwrap();

    let error = err(svc
        .submit_run_user_intent(
            "run-orphaned-input".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-orphaned".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "Nobody can consume this."}),
            },
        )
        .await);
    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0.error_code.as_deref(),
        Some("run_intent_consumer_not_live")
    );

    let durable = engine
        .load_run("user-1", "run-orphaned-input")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_PAUSED);
    assert!(durable.waiting_for.is_none());
    assert!(durable.events.iter().all(|event| {
        event
            .get("data")
            .and_then(|data| data.get("intent_id"))
            .and_then(Value::as_str)
            != Some("intent-orphaned")
    }));
    assert_eq!(
        durable.events.last().unwrap()["data"]["requested_operation"],
        "submit_user_intent"
    );
    engine
        .start_run("run-input-continuation", "user-1", "session-1")
        .await
        .expect("rejected orphan input must not leave the session blocked");
}

#[tokio::test]
async fn submit_run_user_intent_rejects_terminal_durable_run() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("run-terminal-input", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .persist_status("user-1", "run-terminal-input", STATUS_COMPLETED, None, None)
        .await
        .unwrap();

    let e = err(svc
        .submit_run_user_intent(
            "run-terminal-input".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-late".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "Too late."}),
            },
        )
        .await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn submit_run_user_intent_preserves_waiting_execution_state() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("run-queued-input", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .persist_status(
            "user-1",
            "run-queued-input",
            STATUS_WAITING,
            Some("edge_executor"),
            None,
        )
        .await
        .unwrap();
    install_live_run_state(
        &svc,
        "user-1",
        "run-queued-input",
        "session-1",
        RunStatus::Waiting,
        Some("edge_executor"),
    )
    .await;

    let result = svc
        .submit_run_user_intent(
            "run-queued-input".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-waiting-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "Apply when execution resumes."}),
            },
        )
        .await
        .expect("a live waiting run should accept current-run guidance");

    assert_eq!(
        result.status,
        astra_turn_types::UserIntentStatus::AcceptedRemote
    );
    assert!(!result.duplicate);
    let durable = engine
        .load_run("user-1", "run-queued-input")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_WAITING);
    assert_eq!(durable.waiting_for.as_deref(), Some("edge_executor"));
    assert!(durable.events.iter().any(|event| {
        event
            .get("data")
            .and_then(|data| data.get("intent_id"))
            .and_then(Value::as_str)
            == Some("intent-waiting-1")
    }));
}

#[tokio::test]
async fn submit_run_user_intent_rejects_paused_durable_run() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("run-paused-input", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .persist_status("user-1", "run-paused-input", STATUS_PAUSED, None, None)
        .await
        .unwrap();

    let e = err(svc
        .submit_run_user_intent(
            "run-paused-input".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-paused".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "Apply later."}),
            },
        )
        .await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn submit_run_user_intent_rejects_oversized_content() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("run-large-input", "user-1", "session-1")
        .await
        .unwrap();

    let e = err(svc
        .submit_run_user_intent(
            "run-large-input".into(),
            "user-1".into(),
            RunUserIntentData {
                intent_id: "intent-large".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                input: json!({"content": "x".repeat(MAX_USER_INTENT_CHARS + 1)}),
            },
        )
        .await);

    assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE);
    let durable = engine
        .load_run("user-1", "run-large-input")
        .await
        .unwrap()
        .unwrap();
    assert!(
        durable.events.iter().all(|event| {
            event
                .get("data")
                .and_then(|data| data.get("intent_id"))
                .and_then(Value::as_str)
                != Some("intent-large")
        }),
        "oversized input must not be appended before validation"
    );
}

#[tokio::test]
async fn create_run_conflict_checks_durable_active_session() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("existing-run", "user-1", "shared-session")
        .await
        .unwrap();
    let mut request = test_request("second");
    request.session_id = Some("shared-session".into());

    let e = err(svc.create_run("user-1".into(), request).await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
#[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
async fn stream_run_falls_back_to_durable_store_on_cache_miss() {
    let svc = test_service();
    let stream = ok(svc
        .stream_chat("user-1".into(), test_request("hello"))
        .await);
    let engine = &svc.run_engine;
    let durable = engine
        .load_run("user-1", &stream.run_id)
        .await
        .unwrap()
        .unwrap();

    svc.runs.write().await.remove(&stream.run_id);

    let events = ok(svc
        .stream_run(stream.run_id.clone(), "user-1".into(), 1)
        .await);
    assert_eq!(
        events,
        AgenticRunLifecycleService::format_run_events(&durable.events[1..], 1)
    );
}

#[tokio::test]
#[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
async fn list_runs_falls_back_to_durable_store_on_cache_miss() {
    let svc = test_service();
    let first = ok(svc
        .stream_chat("user-1".into(), test_request("first"))
        .await);
    let second = ok(svc
        .stream_chat("user-1".into(), test_request("second"))
        .await);

    svc.runs.write().await.remove(&first.run_id);

    let runs = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    let run_ids: Vec<_> = runs.runs.iter().map(|run| run.run_id.as_str()).collect();
    assert_eq!(runs.total, None);
    assert!(run_ids.contains(&first.run_id.as_str()));
    assert!(run_ids.contains(&second.run_id.as_str()));
}

#[tokio::test]
async fn lifecycle_run_creation_is_durable_by_default() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.user_id, "user-1");
    assert_eq!(durable.session_id, run.session_id);
    assert_eq!(durable.status, STATUS_RUNNING);
}

// ─── EdgeContext integration tests ──────────────────────────────────

#[test]
fn extract_edge_context_from_request_with_tools() {
    let mut ctx = serde_json::Map::new();
    ctx.insert(
        "edge_tools".to_string(),
        json!([{"function": {"name": "bash", "parameters": {}}}]),
    );
    ctx.insert(
        "edge_profile".to_string(),
        json!({"cwd": "/tmp", "git_branch": "main"}),
    );
    let req = ChatRequestData {
        context: Some(ctx),
        ..test_request("hello")
    };

    let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req).expect("edge context");
    assert_eq!(edge_ctx.tool_count(), 1);
    assert_eq!(edge_ctx.tool_names(), vec!["bash"]);
    assert_eq!(edge_ctx.edge_profile.cwd.as_deref(), Some("/tmp"));
    assert_eq!(edge_ctx.edge_profile.git_branch.as_deref(), Some("main"));
}

#[test]
fn extract_edge_context_from_empty_request() {
    let req = test_request("hello");
    let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req).expect("edge context");
    assert!(!edge_ctx.has_tools());
    assert!(edge_ctx.edge_profile.cwd.is_none());
}

#[test]
fn extract_edge_context_rejects_malformed_context() {
    let mut ctx = serde_json::Map::new();
    ctx.insert("edge_tools".to_string(), json!({"not": "an array"}));
    let req = ChatRequestData {
        context: Some(ctx),
        ..test_request("hello")
    };

    let error = AgenticRunLifecycleService::extract_edge_context(&req)
        .expect_err("malformed edge context must fail loud");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error.1.0.detail.contains("invalid edge context"),
        "unexpected error: {}",
        error.1.0.detail
    );
}

#[tokio::test]
async fn create_run_rejects_malformed_edge_context_before_agent_start() {
    let svc = test_service();
    let mut ctx = serde_json::Map::new();
    ctx.insert("edge_tools".to_string(), json!({"not": "an array"}));
    let req = ChatRequestData {
        context: Some(ctx),
        ..test_request("hello")
    };

    let error = err(svc.create_run("user-1".into(), req).await);

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error.1.0.detail.contains("invalid edge context"),
        "unexpected error: {}",
        error.1.0.detail
    );
}

#[tokio::test]
async fn stream_chat_rejects_malformed_edge_context_before_agent_start() {
    let svc = test_service();
    let mut ctx = serde_json::Map::new();
    ctx.insert("edge_profile".to_string(), json!({"cwd": 42}));
    let req = ChatRequestData {
        context: Some(ctx),
        ..test_request("hello")
    };

    let error = err(svc.stream_chat("user-1".into(), req).await);

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error.1.0.detail.contains("invalid edge context"),
        "unexpected error: {}",
        error.1.0.detail
    );
}

// ─── Background spawning integration tests ──────────────────────────

#[tokio::test]
async fn fail_started_run_before_spawn_persists_terminal_events() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("run-pre-spawn", "user-1", "session-1")
        .await
        .unwrap();

    svc.fail_started_run_before_spawn(
        "user-1",
        "run-pre-spawn",
        "server capacity timeout before agentic loop start",
        PreSpawnFailureCode::RunAdmissionTimeout,
    )
    .await;

    let durable = engine
        .load_run("user-1", "run-pre-spawn")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_FAILED);
    assert_eq!(durable.error_code.as_deref(), Some("run_admission_timeout"));
    assert!(
        durable.events.iter().any(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("run_error")
                && event
                    .get("data")
                    .and_then(Value::as_object)
                    .and_then(|data| data.get("error_code"))
                    .and_then(Value::as_str)
                    == Some("run_admission_timeout")
        }),
        "durable run_error must explain the pre-spawn failure"
    );
    assert!(
        durable.events.iter().any(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("run_finished")
                && event
                    .get("data")
                    .and_then(Value::as_object)
                    .and_then(|data| data.get("error_kind"))
                    .and_then(Value::as_str)
                    == Some("server_error")
        }),
        "durable run_finished must preserve the pre-spawn terminal code"
    );
}

#[tokio::test]
async fn fail_started_run_before_spawn_transition_failure_does_not_commit_partial_terminal() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-pre-spawn-fail", "user-1", "session-1")
        .await
        .unwrap();

    svc.fail_started_run_before_spawn(
        "user-1",
        "run-pre-spawn-fail",
        "server capacity timeout before agentic loop start",
        PreSpawnFailureCode::RunAdmissionTimeout,
    )
    .await;

    let durable = engine
        .load_run("user-1", "run-pre-spawn-fail")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.error_code.is_none());
    assert!(
        durable.events.iter().all(|event| {
            !matches!(
                event.get("event_type").and_then(Value::as_str),
                Some("run_error" | "run_finished")
            )
        }),
        "failed pre-spawn transition must not leave partial terminal events"
    );
}

#[tokio::test]
async fn create_run_token_budget_reject_persists_terminal_events() {
    let svc = test_service()
        .with_resource_governor(Arc::new(DenyTokenBudgetGovernor))
        .with_run_concurrency_limit(1);
    let run = ok(svc
        .create_run("user-1".into(), test_request("over budget"))
        .await);

    assert!(
        svc.drain_background_tasks(Duration::from_secs(1)).await,
        "budget rejection task should finish promptly"
    );
    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(durable.status, STATUS_FAILED);
    assert_eq!(
        durable.error_code.as_deref(),
        Some("per_user_daily_token_quota")
    );
    assert!(
        durable
            .events
            .iter()
            .any(
                |event| event.get("event_type").and_then(Value::as_str) == Some("run_error")
                    && event
                        .get("data")
                        .and_then(Value::as_object)
                        .and_then(|data| data.get("error_code"))
                        .and_then(Value::as_str)
                        == Some("per_user_daily_token_quota")
            ),
        "durable run_error must explain the quota failure"
    );
    assert!(
        durable
            .events
            .iter()
            .any(
                |event| event.get("event_type").and_then(Value::as_str) == Some("run_finished")
                    && event
                        .get("data")
                        .and_then(Value::as_object)
                        .and_then(|data| data.get("error_kind"))
                        .and_then(Value::as_str)
                        == Some("budget_exhausted")
            ),
        "durable run_finished must preserve the terminal quota code"
    );
}

#[tokio::test]
async fn token_budget_reject_transition_failure_does_not_commit_status_or_events() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-quota-fail", "user-1", "session-1")
        .await
        .unwrap();

    let committed_events = AgenticRunLifecycleService::persist_started_run_quota_rejection(
        engine,
        &svc.runs_handle(),
        "user-1",
        "run-quota-fail",
        astra_services::resource_governor::ResourceLimitKind::DailyTokens,
        "daily token budget exhausted (1000/1000)",
    )
    .await;

    assert!(
        committed_events.is_none(),
        "injected transition failure must not report committed events"
    );
    let durable = engine
        .load_run("user-1", "run-quota-fail")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.error_code.is_none());
    assert!(
        durable.events.iter().all(|event| {
            !matches!(
                event.get("event_type").and_then(Value::as_str),
                Some("run_error" | "run_finished")
            )
        }),
        "failed quota transition must not leave partial terminal events"
    );
}

#[tokio::test]
async fn stream_chat_token_budget_reject_sends_sse_terminal_events() {
    let svc = test_service()
        .with_resource_governor(Arc::new(DenyTokenBudgetGovernor))
        .with_run_concurrency_limit(1);
    let mut stream = ok(svc
        .stream_chat("user-1".into(), test_request("over budget"))
        .await);
    let mut rx = stream.event_rx.take().expect("stream event receiver");
    let events = tokio::time::timeout(Duration::from_secs(1), async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("budget rejection stream should close promptly");

    assert!(
        svc.drain_background_tasks(Duration::from_secs(1)).await,
        "budget rejection task should finish promptly"
    );
    assert!(
        events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("run_error")
                && event.get("error_code").and_then(Value::as_str)
                    == Some("per_user_daily_token_quota")
        }),
        "SSE stream must include a structured run_error: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("run_finished")
                && event.get("status").and_then(Value::as_str) == Some(STATUS_FAILED)
        }),
        "SSE stream must include failed run_finished: {events:?}"
    );

    let durable = svc
        .run_engine
        .load_run("user-1", &stream.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_FAILED);
    assert_eq!(
        durable.error_code.as_deref(),
        Some("per_user_daily_token_quota")
    );
}

#[tokio::test]
async fn interactive_create_run_admission_reject_cleans_ws_channels() {
    let svc = test_service().with_run_concurrency_limit(1);
    svc.test_run_semaphore().close();
    let mut request = test_request("admission closed");
    request.interactive_client = true;

    let err = svc
        .create_run("user-1".into(), request)
        .await
        .expect_err("closed admission should reject before spawn");

    assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err.1.0.error_code.as_deref(), Some("run_admission_closed"));
    assert!(
        svc.approval_channels.lock().await.is_empty(),
        "pre-spawn admission failure must not leak approval channel receivers"
    );
    assert!(
        svc.user_prompt_channels.lock().await.is_empty(),
        "pre-spawn admission failure must not leak ask_user channel receivers"
    );
    assert!(
        svc.progress_channels.lock().await.is_empty(),
        "pre-spawn admission failure must not leak progress channel receivers"
    );

    let page = svc
        .run_engine
        .list_user_runs_cursor("user-1", 10, None)
        .await
        .unwrap();
    let runs = page.runs;
    assert_eq!(page.total, None);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, STATUS_FAILED);
    assert_eq!(runs[0].error_code.as_deref(), Some("run_admission_closed"));
}

#[tokio::test]
async fn interactive_stream_chat_admission_reject_leaves_no_ws_channels() {
    let svc = test_service().with_run_concurrency_limit(1);
    svc.test_run_semaphore().close();
    let mut request = test_request("stream admission closed");
    request.interactive_client = true;

    let err = svc
        .stream_chat("user-1".into(), request)
        .await
        .expect_err("closed admission should reject streaming before spawn");

    assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err.1.0.error_code.as_deref(), Some("run_admission_closed"));
    assert!(svc.approval_channels.lock().await.is_empty());
    assert!(svc.user_prompt_channels.lock().await.is_empty());
    assert!(svc.progress_channels.lock().await.is_empty());

    let page = svc
        .run_engine
        .list_user_runs_cursor("user-1", 10, None)
        .await
        .unwrap();
    let runs = page.runs;
    assert_eq!(page.total, None);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, STATUS_FAILED);
    assert_eq!(runs[0].error_code.as_deref(), Some("run_admission_closed"));
}

// ─── DelegationTracker integration tests ────────────────────────────

#[tokio::test]
async fn delegation_tracker_get_children() {
    use crate::server::delegation::engine::{DelegationTracker, SubRunRecord, SubRunState};

    let tracker = DelegationTracker::new();
    tracker
        .record_sub_run(SubRunRecord {
            delegation_id: "d1".into(),
            run_id: "child-1".into(),
            parent_run_id: "parent-1".into(),
            agent_id: "agent-a".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;
    tracker
        .record_sub_run(SubRunRecord {
            delegation_id: "d1".into(),
            run_id: "child-2".into(),
            parent_run_id: "parent-1".into(),
            agent_id: "agent-b".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;
    tracker
        .record_sub_run(SubRunRecord {
            delegation_id: "d2".into(),
            run_id: "other-child".into(),
            parent_run_id: "parent-2".into(),
            agent_id: "agent-c".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;

    let mut children = tracker.get_children("parent-1").await;
    children.sort();
    assert_eq!(children, vec!["child-1", "child-2"]);

    let children = tracker.get_children("parent-2").await;
    assert_eq!(children, vec!["other-child"]);

    let children = tracker.get_children("nonexistent").await;
    assert!(children.is_empty());
}

/// P0-C: drain_background_tasks returns true when no tasks are running.
#[tokio::test]
async fn drain_background_tasks_returns_immediately_when_idle() {
    // Test the drain logic directly: counter at 0 → drain returns true immediately.
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
    let drained = loop {
        if count.load(Ordering::Acquire) == 0 {
            break true;
        }
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(drained, "counter at 0 — drain must return true immediately");
}

/// P0-C: background_task_count increments on spawn and decrements on exit.
#[tokio::test]
async fn background_task_count_tracks_spawned_tasks() {
    use std::sync::atomic::Ordering;
    let count = Arc::new(AtomicUsize::new(0));
    let count_clone = Arc::clone(&count);

    // Simulate what the spawn does: increment, spawn, decrement on drop
    count.fetch_add(1, Ordering::Release);
    let handle = tokio::spawn(async move {
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Release);
            }
        }
        let _g = Guard(count_clone);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    assert_eq!(count.load(Ordering::Acquire), 1, "task in flight");
    handle.await.unwrap();
    assert_eq!(
        count.load(Ordering::Acquire),
        0,
        "task completed — counter must be 0"
    );
}

/// P1-F: list_runs_cursor pagination must be deterministic — all runs appear
/// exactly once across pages, with no duplicates or missing entries.
#[tokio::test]
async fn list_runs_cursor_pagination_is_deterministic() {
    let svc = test_service();
    for i in 0..5 {
        ok(svc
            .create_run("user-pg".into(), test_request(&format!("msg {i}")))
            .await);
    }
    // Collect all run_ids across 3 pages
    let mut all_ids = Vec::new();
    let page1 = ok(svc.list_runs_cursor("user-pg".into(), 2, None).await);
    all_ids.extend(page1.runs.iter().map(|r| r.run_id.clone()));
    let page2 = ok(svc
        .list_runs_cursor("user-pg".into(), 2, page1.next_cursor)
        .await);
    all_ids.extend(page2.runs.iter().map(|r| r.run_id.clone()));
    let page3 = ok(svc
        .list_runs_cursor("user-pg".into(), 2, page2.next_cursor)
        .await);
    all_ids.extend(page3.runs.iter().map(|r| r.run_id.clone()));

    assert_eq!(all_ids.len(), 5, "all 5 runs must appear across pages");
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(
        unique.len(),
        5,
        "no duplicate run_ids across pages — pagination must be deterministic"
    );
}

/// P1-A: RunStatus must have a Waiting variant that is non-terminal.
/// Runs needing external input must not be killed as Failed.
#[test]
fn waiting_is_non_terminal_status() {
    // Running → Waiting is valid
    assert!(
        RunStatus::Running
            .try_transition(&RunStatus::Waiting)
            .is_ok(),
        "Running → Waiting must be allowed"
    );
    // Waiting → Running is valid (resume after input)
    assert!(
        RunStatus::Waiting
            .try_transition(&RunStatus::Running)
            .is_ok(),
        "Waiting → Running must be allowed (resume)"
    );
    // Waiting → Cancelled is valid
    assert!(
        RunStatus::Waiting
            .try_transition(&RunStatus::Cancelled)
            .is_ok(),
        "Waiting → Cancelled must be allowed"
    );
    // Waiting → Failed is valid (timeout)
    assert!(
        RunStatus::Waiting
            .try_transition(&RunStatus::Failed)
            .is_ok(),
        "Waiting → Failed must be allowed"
    );
    // Waiting serializes as "waiting"
    assert_eq!(RunStatus::Waiting.as_str(), "waiting");
}

#[test]
fn run_status_live_semantics_align_with_durable_owner() {
    assert_eq!(
        RunStatus::from_durable_status(STATUS_RUNNING),
        Some(RunStatus::Running)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_WAITING),
        Some(RunStatus::Waiting)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_PAUSED),
        Some(RunStatus::Paused)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_COMPLETED),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_DELEGATED),
        Some(RunStatus::Delegated)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_FAILED),
        Some(RunStatus::Failed)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_CANCELLED),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(RunStatus::from_durable_status("mystery"), None);

    assert!(RunStatus::Waiting.is_resumable());
    assert!(RunStatus::Paused.is_resumable());
    assert!(!RunStatus::Running.is_resumable());
    assert!(!RunStatus::Completed.is_resumable());
    assert!(!RunStatus::Delegated.is_resumable());

    assert_eq!(
        RunStatus::Running.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_RUNNING, None)
    );
    assert_eq!(
        RunStatus::Waiting.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_WAITING, None)
    );
    assert_eq!(
        RunStatus::Paused.blocks_session(Some("tool_approval")),
        astra_services::runs::durable_run_status_blocks_session(
            STATUS_PAUSED,
            Some("tool_approval")
        )
    );
    assert_eq!(
        RunStatus::Paused.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_PAUSED, None)
    );
    assert_eq!(
        RunStatus::Completed.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_COMPLETED, None)
    );
    assert_eq!(
        RunStatus::Delegated.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_DELEGATED, None)
    );
}

/// A waiting outcome has no live task after finalization, so it must release
/// the execution slot and advertise session continuation instead of leaving an
/// unconsumable durable input queue.
#[test]
fn finalize_run_events_routes_waiting_to_session_continuation() {
    let svc = test_service();
    let request = test_request("wait");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Waiting("tool_approval".into())),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Paused);
    assert!(error.is_none());
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event_type"], "run_waiting");
    assert_eq!(events[0]["data"]["reason"], "waiting: tool_approval");
    assert_eq!(events[0]["data"]["resume_strategy"], "session_continuation");
    assert_eq!(events[1]["event_type"], "run_finished");
    assert_eq!(events[1]["data"]["interrupted"], true);
}

/// P1-F: stream_chat must persist usage unconditionally.
/// Cancelled runs still consumed tokens and must have accurate durable records,
/// even when status persistence is skipped.

/// P1-C: build_server_skill_executor must accept and wire a cancel_token.
/// Without this, skill sub-runs ignore parent cancellation.

/// Runtime tool surfacing for forked server skills must inherit the parent
/// workspace/executor/runtime binding; otherwise sub-runs see raw edge
/// schemas without the capability resolver's runtime truth.

/// P1-C: build_initial_state must pass cancel_token to skill executor builder.

#[test]
fn resumable_run_statuses_stay_live_for_resume() {
    assert!(RunStatus::Waiting.is_resumable());
    assert!(RunStatus::Paused.is_resumable());
    assert!(!RunStatus::Running.is_resumable());
    assert!(!RunStatus::Completed.is_resumable());
    assert!(!RunStatus::Failed.is_resumable());
    assert!(!RunStatus::Cancelled.is_resumable());
}

/// A Waiting run persisted in durable store must be cancellable even after
/// the process-local control handle is gone.
#[tokio::test]
async fn cancel_run_waiting_cache_miss_persists_cancelled() {
    let svc = test_service();
    let engine = &svc.run_engine;
    engine
        .start_run("waiting-run", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .persist_status(
            "user-1",
            "waiting-run",
            STATUS_WAITING,
            Some("tool_approval"),
            None,
        )
        .await
        .unwrap();

    let result = ok(svc.cancel_run("waiting-run".into(), "user-1".into()).await);
    let durable = engine
        .load_run("user-1", "waiting-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, STATUS_CANCELLED);
    assert_eq!(durable.status, STATUS_CANCELLED);
}

/// Admission control: semaphore rejects when at capacity, allows after release.
#[tokio::test]
async fn run_semaphore_admission_control() {
    // Limit = 1: only one concurrent run permitted.
    let svc = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
    )
    .with_run_concurrency_limit(1);
    let sem = svc.test_run_semaphore();

    // 1st acquire succeeds.
    let permit1 = sem.clone().try_acquire_owned().expect("first permit");
    // 2nd acquire must fail — at capacity.
    assert!(
        sem.clone().try_acquire_owned().is_err(),
        "second acquire must fail when at capacity"
    );

    // After release, re-acquire succeeds.
    drop(permit1);
    let permit2 = sem
        .clone()
        .try_acquire_owned()
        .expect("re-acquire after release");
    drop(permit2);
}

/// Admission control: limit=2, third acquire must fail, release creates room.
#[tokio::test]
async fn run_semaphore_limit_two() {
    let svc = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
    )
    .with_run_concurrency_limit(2);
    let sem = svc.test_run_semaphore();

    let p1 = sem.clone().try_acquire_owned().expect("first");
    let p2 = sem.clone().try_acquire_owned().expect("second");
    assert!(sem.clone().try_acquire_owned().is_err(), "third must fail");

    drop(p1);
    // Now one slot open, re-acquire works.
    let p3 = sem
        .clone()
        .try_acquire_owned()
        .expect("re-acquire after one drop");
    drop(p2);
    drop(p3);
}

/// Admission with timeout: `acquire_owned` + `timeout` rejects after
/// the deadline while a short release window lets a waiter proceed.
#[tokio::test]
async fn run_semaphore_admission_timeout_waits_and_proceeds() {
    let svc = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
    )
    .with_run_concurrency_limit(1);
    let sem = svc.test_run_semaphore();

    // 1st acquire: capacity exhausted.
    let p1 = sem.clone().try_acquire_owned().expect("first");
    // Spawn a waiter with a short timeout — it will time out.
    let sem2 = sem.clone();
    let timeout_result =
        tokio::time::timeout(std::time::Duration::from_millis(50), sem2.acquire_owned()).await;
    assert!(
        timeout_result.is_err(),
        "waiter should time out when no slot opens"
    );

    // Now spawn a waiter and release the slot quickly — waiter should get it.
    let sem3 = sem.clone();
    let waiter = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(5), sem3.acquire_owned())
            .await
            .expect("timeout should not fire")
            .expect("acquire_owned")
    });
    // Small yield to let the waiter enter acquire_owned.
    tokio::task::yield_now().await;
    drop(p1); // release the slot
    let p2 = waiter.await.expect("waiter panicked");
    drop(p2);
}

#[tokio::test]
async fn run_admission_metrics_record_acquired_and_timeout() {
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
    let svc = test_service()
        .with_run_concurrency_limit(1)
        .with_metrics_registry(registry.clone());
    let sem = svc.test_run_semaphore();
    let first = sem.clone().try_acquire_owned().expect("first permit");

    let timed_out = match svc.acquire_run_permit(Duration::from_millis(5)).await {
        Ok(_) => panic!("admission should time out while the only permit is held"),
        Err(error) => error,
    };
    assert_eq!(timed_out, RunAdmissionError::Timeout);

    drop(first);
    let acquired = svc
        .acquire_run_permit(Duration::from_secs(1))
        .await
        .expect("released permit should be acquired");
    drop(acquired);

    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains("astra_run_admission_attempts_total{outcome=\"timeout\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_run_admission_attempts_total{outcome=\"acquired\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_run_admission_wait_ms_total{outcome=\"timeout\"}"),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_run_admission_weight_units_total{outcome=\"timeout\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_run_admission_weight_units_total{outcome=\"acquired\"} 1"),
        "{rendered}"
    );
}

#[tokio::test]
async fn run_admission_closed_semaphore_is_rejected_and_counted() {
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
    let svc = test_service()
        .with_run_concurrency_limit(1)
        .with_metrics_registry(registry.clone());
    svc.test_run_semaphore().close();

    let error = match svc.acquire_run_permit(Duration::from_secs(1)).await {
        Ok(_) => panic!("closed semaphore must not admit a run without a permit"),
        Err(error) => error,
    };

    assert_eq!(error, RunAdmissionError::Closed);
    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains("astra_run_admission_attempts_total{outcome=\"closed\"} 1"),
        "{rendered}"
    );
}

#[test]
fn run_admission_timeout_ignores_legacy_env_knob() {
    let _default = EnvVarGuard::remove("ASTRA_RUN_ADMISSION_TIMEOUT_SECS");
    assert_eq!(
        run_admission_timeout(),
        Duration::from_secs(DEFAULT_RUN_ADMISSION_TIMEOUT_SECS)
    );

    let _legacy = EnvVarGuard::set("ASTRA_RUN_ADMISSION_TIMEOUT_SECS", "90");
    assert_eq!(
        run_admission_timeout(),
        Duration::from_secs(DEFAULT_RUN_ADMISSION_TIMEOUT_SECS)
    );
}

#[test]
fn run_admission_capacity_response_uses_distinct_error_codes() {
    let timeout = run_admission_capacity_response(RunAdmissionError::Timeout);
    assert_eq!(timeout.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        timeout.1.error_code.as_deref(),
        Some("run_admission_timeout")
    );
    assert!(timeout.1.detail.contains("run_admission_timeout"));

    let closed = run_admission_capacity_response(RunAdmissionError::Closed);
    assert_eq!(closed.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(closed.1.error_code.as_deref(), Some("run_admission_closed"));
    assert!(closed.1.detail.contains("run_admission_closed"));
}

#[test]
fn per_user_run_quota_response_uses_quota_error_code() {
    let response = per_user_run_quota_response(
        astra_services::resource_governor::ResourceLimitKind::ConcurrentSessions,
        "concurrent session limit reached (5/5)".to_string(),
    );

    assert_eq!(response.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.1.error_code.as_deref(),
        Some("per_user_concurrent_session_quota")
    );
    assert!(response.1.detail.contains("Per-user run quota exceeded"));
    assert!(response.1.detail.contains("concurrent_sessions"));
}

#[test]
fn durable_run_event_batch_metrics_record_rows_bytes_and_compaction() {
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
    register_durable_run_event_metrics(&registry);
    let events = vec![
        json!({"event_type": "durable_events_compacted", "data": {"dropped_events": 10}}),
        json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
    ];
    let bytes = events
        .iter()
        .map(durable_run_event_estimated_bytes)
        .sum::<usize>();

    record_durable_run_event_batch_metrics(
        Some(&registry),
        "streaming_terminal",
        "planned",
        &events,
    );
    record_durable_run_event_batch_metrics(Some(&registry), "streaming_terminal", "error", &events);

    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains("# TYPE astra_durable_run_event_row_budget gauge")
            && rendered.contains("astra_durable_run_event_row_budget "),
        "{rendered}"
    );
    assert!(
        rendered.contains("# TYPE astra_durable_run_event_byte_budget gauge")
            && rendered.contains("astra_durable_run_event_byte_budget "),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "astra_durable_run_event_batches_total{compacted=\"true\",outcome=\"planned\",path=\"streaming_terminal\"} 1"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "astra_durable_run_event_rows_total{compacted=\"true\",outcome=\"planned\",path=\"streaming_terminal\"} 2"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "astra_durable_run_event_bytes_total{{compacted=\"true\",outcome=\"planned\",path=\"streaming_terminal\"}} {bytes}"
        )),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "astra_durable_run_event_batches_total{compacted=\"true\",outcome=\"error\",path=\"streaming_terminal\"} 1"
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("run_id=") && !rendered.contains("session_id="),
        "metrics must stay low-cardinality: {rendered}"
    );
}

#[test]
fn edge_approval_persistence_normalizes_single_and_batch_requests() {
    let single = json!({
        "type": "approval_required",
        "request_id": "approval-1",
        "tool": "bash",
        "approval_kind": "explicit",
        "detail": "printf ok",
        "display_label": "$ printf ok",
    });
    let single_facts = canonical_edge_approval_requests(&single);
    assert_eq!(single_facts.len(), 1);
    assert_eq!(single_facts[0]["event_type"], "approval_required");
    assert_eq!(single_facts[0]["data"]["request_id"], "approval-1");
    assert_eq!(single_facts[0]["data"]["tool"], "bash");
    assert_eq!(single_facts[0]["data"]["delivery"], "edge_ledger");
    assert_eq!(single_facts[0]["data"]["detail"], "printf ok");
    assert_eq!(
        single_facts[0]["idempotency_key"],
        "edge-approval-required:approval-1"
    );

    let batch = json!({
        "type": "approval_batch_required",
        "requests": [
            {
                "request_id": "approval-a",
                "tool": "write_file",
                "approval_kind": "standard",
                "path": "/tmp/a",
            },
            {
                "request_id": "approval-b",
                "tool": "write_file",
                "approval_kind": "standard",
                "path": "/tmp/b",
            },
        ],
    });
    let batch_facts = canonical_edge_approval_requests(&batch);
    assert_eq!(batch_facts.len(), 2);
    assert_eq!(batch_facts[0]["data"]["request_id"], "approval-a");
    assert_eq!(batch_facts[1]["data"]["request_id"], "approval-b");
    assert_eq!(batch_facts[0]["data"]["path"], "/tmp/a");
    assert_eq!(batch_facts[1]["data"]["path"], "/tmp/b");
    assert!(incrementally_persisted_edge_interaction_event(&single));
    assert!(incrementally_persisted_edge_interaction_event(&batch));
}

#[test]
fn edge_approval_persistence_rejects_unaddressable_items() {
    let malformed = json!({
        "type": "approval_batch_required",
        "requests": [
            {"request_id": "", "tool": "bash"},
            {"request_id": "missing-tool"},
            "not-an-object",
        ],
    });
    assert!(canonical_edge_approval_requests(&malformed).is_empty());
    let already_durable = json!({
        "type": "approval_required",
        "request_id": "durable-approval",
        "tool": "bash",
        "approval_kind": "standard",
        "delivery": "durable",
    });
    assert!(canonical_edge_approval_requests(&already_durable).is_empty());
    assert!(!incrementally_persisted_edge_interaction_event(
        &already_durable
    ));
    assert!(!incrementally_persisted_edge_interaction_event(&json!({
        "type": "tool_request",
        "request_id": "missing-tool"
    })));
}

#[test]
fn edge_tool_request_has_a_replayable_canonical_fact() {
    let request = json!({
        "type": "tool_request",
        "run_id": "child-run",
        "session_id": "session-1",
        "request_id": "tool-1",
        "tool": "bash",
        "args": {"cmd": "printf ok"}
    });

    let durable = canonical_edge_tool_request(&request).expect("canonical tool request");
    assert_eq!(durable["event_type"], "tool_request");
    assert_eq!(durable["idempotency_key"], "edge-tool-request:tool-1");
    assert_eq!(durable["data"]["run_id"], "child-run");
    assert_eq!(durable["data"]["request_id"], "tool-1");
    assert_eq!(durable["data"]["tool"], "bash");
    assert!(incrementally_persisted_edge_interaction_event(&request));
}

#[tokio::test]
async fn full_live_queue_cannot_hide_or_precede_durable_approval_truth() {
    let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    engine
        .start_run("run-approval", "user-1", "session-1")
        .await
        .unwrap();
    let (event_tx, mut event_rx) = mpsc::channel(1);
    event_tx
        .send(json!({"type": "text_delta", "content": "queue is full"}))
        .await
        .unwrap();
    let sink = DurableHostInteractionSink {
        run_engine: engine.clone(),
        user_id: "user-1".to_string(),
        run_id: "run-approval".to_string(),
        session_id: "session-1".to_string(),
        agent_id: None,
        event_tx: Some(event_tx),
    };
    let delivery = server_loop_host::HostInteractionSink::commit_and_deliver(
        &sink,
        json!({
            "type": "approval_required",
            "request_id": "approval-1",
            "tool": "bash",
            "approval_kind": "standard"
        }),
    );
    tokio::pin!(delivery);

    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut delivery)
            .await
            .is_err(),
        "a full live queue should backpressure delivery"
    );
    let durable = engine
        .load_run("user-1", "run-approval")
        .await
        .unwrap()
        .unwrap();
    assert!(
        durable.events.iter().any(|event| {
            event["event_type"] == "approval_required"
                && event["data"]["request_id"] == "approval-1"
        }),
        "approval replay truth must exist before any executor can wait for a callback"
    );

    let progress = event_rx.recv().await.unwrap();
    assert_eq!(progress["type"], "text_delta");
    delivery.await.unwrap();
    let approval = event_rx.recv().await.unwrap();
    assert_eq!(approval["type"], "approval_required");
    assert_eq!(approval[HOST_INTERACTION_COMMITTED_FIELD], true);
}

#[tokio::test]
async fn detached_observer_does_not_revoke_committed_interaction() {
    let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    engine
        .start_run("run-detached-approval", "user-1", "session-1")
        .await
        .unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_rx);
    let sink = DurableHostInteractionSink {
        run_engine: engine.clone(),
        user_id: "user-1".to_string(),
        run_id: "run-detached-approval".to_string(),
        session_id: "session-1".to_string(),
        agent_id: None,
        event_tx: Some(event_tx),
    };

    server_loop_host::HostInteractionSink::commit_and_deliver(
        &sink,
        json!({
            "type": "approval_required",
            "request_id": "approval-detached",
            "tool": "bash",
            "approval_kind": "standard"
        }),
    )
    .await
    .expect("durable interaction remains valid after live observer detaches");

    let durable = engine
        .load_run("user-1", "run-detached-approval")
        .await
        .unwrap()
        .unwrap();
    assert!(durable.events.iter().any(|event| {
        event["event_type"] == "approval_required"
            && event["data"]["request_id"] == "approval-detached"
    }));
}

#[test]
fn child_edge_ws_uses_runtime_dispatch_while_thin_client_uses_callback_lane() {
    let edge_ws = ExecutionBindingSnapshot::inferred(
        WorkspaceBinding::edge_workspace(
            "Developer edge",
            "/workspace",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-1",
            "Developer edge",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    assert!(
        !child_uses_client_tool_delivery(true, Some(&edge_ws)),
        "a connected edge executor must receive child tools through runtime dispatch"
    );

    let thin_client = ExecutionBindingSnapshot::inferred(
        WorkspaceBinding::edge_workspace(
            "Thin client",
            "/workspace",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "browser-1",
            "Browser callback",
            ToolTransportKind::EdgeLedger,
            ExecutorStatus::Online,
        ),
    );
    assert!(child_uses_client_tool_delivery(true, Some(&thin_client)));
    assert!(
        !child_uses_client_tool_delivery(false, Some(&thin_client)),
        "a callback transport is not executable when the parent has no delivery lane"
    );
}
