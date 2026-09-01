//! Deferred post-commit projections for an already-durable turn.

use std::time::{Duration, Instant};

use super::turn_commit::DeferredTurnSidecarWork;
use crate::cli::notifications;
use crate::cli::session::session_projection::{
    CslCheckpointFields, build_full_session_state_compact,
};
use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;

/// CSL is a local continuation projection of a primary journal turn, not its
/// durability boundary. It runs on the post-commit worker, so latency must not
/// turn into cancellation: dropping an in-flight projection creates an
/// avoidable continuation gap on the next resume.
const SIDECAR_PROJECTION_MAX_ATTEMPTS: usize = 3;
const SIDECAR_PROJECTION_RETRY_BASE: Duration = Duration::from_millis(25);

/// Fully owned work handed from turn settlement to a serialized worker. No
/// mutable `SessionState` crosses this boundary, so a slow disk or server can
/// never keep the next turn from starting.
pub(crate) struct TurnPostCommitJob {
    owner_id: String,
    session_id: Option<String>,
    attachment_epoch: u64,
    expected_cursor: Option<astra_turn_types::SessionCursorV1>,
    turn: u32,
    final_messages: Vec<serde_json::Value>,
    csl_state: astra_turn_core::conversation_log::SessionStateCompact,
    csl_manager: Option<astra_turn_core::conversation_log::manager::CslManager>,
    deferred_sidecars: Option<DeferredTurnSidecarWork>,
    notification: Option<(notifications::NotificationConfig, Duration)>,
    /// Keeps observed full-history payload residency accounted while this job
    /// is waiting in the post-commit queue.
    _queue_bytes: Option<astra_core::history_work::QueueBytesReservation>,
}

/// Result of a post-commit job. The event loop applies it only when the
/// session identity still matches; stale completions cannot overwrite a later
/// resume/rebind.
pub(crate) struct TurnPostCommitCompletion {
    owner_id: String,
    pub(crate) session_id: Option<String>,
    attachment_epoch: u64,
    expected_cursor: Option<astra_turn_types::SessionCursorV1>,
    pub(crate) csl_manager: Option<astra_turn_core::conversation_log::manager::CslManager>,
    pub(crate) errors: Vec<String>,
}

pub(crate) fn prepare_turn_post_commit_job(
    state: &mut SessionState,
    _api: &astra_thin_client::ThinClient,
    _profile: Option<&str>,
    final_messages: Vec<serde_json::Value>,
    csl_checkpoint_fields: CslCheckpointFields,
    turn_start: Instant,
) -> TurnPostCommitJob {
    let expected_cursor = state
        .active_conversation
        .as_ref()
        .map(|conversation| conversation.cursor().clone());
    let final_messages = state
        .active_conversation
        .as_ref()
        .map(|conversation| conversation.materialize())
        .unwrap_or(final_messages);
    let csl_state = state
        .csl_manager
        .as_ref()
        .map(|manager| manager.last_session_state().clone())
        .map(|previous| build_full_session_state_compact(state, csl_checkpoint_fields, &previous))
        .unwrap_or_default();
    let notification = notification_for_turn(state, turn_start.elapsed());
    // The active session keeps its manager while this job is queued. The
    // worker receives a fresh manager and reconciles from durable CSL state,
    // which keeps rapid consecutive turns ordered without leaving a temporary
    // `None` manager that would silently skip later projections.
    let csl_manager = state
        .csl_manager
        .as_ref()
        .and(state.session_id.as_deref())
        .and_then(build_local_csl_manager);
    TurnPostCommitJob {
        owner_id: astra_services::local_owner_scope().id().to_string(),
        session_id: state.session_id.clone(),
        attachment_epoch: state.session_attachment_epoch,
        expected_cursor,
        turn: state.turn,
        final_messages,
        csl_state,
        csl_manager,
        deferred_sidecars: None,
        notification,
        _queue_bytes: None,
    }
}

pub(crate) fn account_turn_post_commit_queue(job: &mut TurnPostCommitJob) {
    if job._queue_bytes.is_none() {
        job._queue_bytes = crate::cli::history_work::reserve_json_history_queue(
            astra_core::history_work::HistoryWorkSite::CliPostCommitQueue,
            &job.final_messages,
        );
    }
}

fn build_local_csl_manager(
    session_id: &str,
) -> Option<astra_turn_core::conversation_log::manager::CslManager> {
    let store = std::sync::Arc::new(
        astra_turn_core::conversation_log::file_store::FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ),
    );
    match astra_turn_core::conversation_log::manager::CslManager::new(
        store,
        session_id.to_string(),
        Default::default(),
    ) {
        Ok(manager) => Some(manager),
        Err(error) => {
            astra_core::agent_warn!(
                "csl",
                "manager init for deferred projection failed: {error}"
            );
            None
        }
    }
}

pub(crate) fn attach_deferred_sidecars(
    job: &mut TurnPostCommitJob,
    deferred_sidecars: Option<DeferredTurnSidecarWork>,
) {
    job.deferred_sidecars = deferred_sidecars;
}

#[tracing::instrument(
    target = "astra_cli::turn_post_commit",
    skip_all,
    fields(session_id = ?job.session_id, turn = job.turn)
)]
pub(crate) async fn execute_turn_post_commit_job(
    mut job: TurnPostCommitJob,
) -> TurnPostCommitCompletion {
    // The worker has dequeued the job; keep execution-time ownership separate
    // from queue residency.
    job._queue_bytes = None;
    let started = Instant::now();
    let session_id = job.session_id.clone();
    let owner_id = job.owner_id.clone();
    let attachment_epoch = job.attachment_epoch;
    let expected_cursor = job.expected_cursor.clone();
    let turn = job.turn;
    let mut errors = Vec::new();
    let sidecar_started = Instant::now();
    if let Some(sidecars) = job.deferred_sidecars.take() {
        let sidecars = std::sync::Arc::new(sidecars);
        for attempt in 0..SIDECAR_PROJECTION_MAX_ATTEMPTS {
            let work = std::sync::Arc::clone(&sidecars);
            match tokio::task::spawn_blocking(move || work.execute(attempt > 0)).await {
                Ok(Ok(())) => break,
                Ok(Err(error))
                    if error.is_retryable() && attempt + 1 < SIDECAR_PROJECTION_MAX_ATTEMPTS =>
                {
                    tokio::time::sleep(SIDECAR_PROJECTION_RETRY_BASE * 2u32.pow(attempt as u32))
                        .await;
                }
                Ok(Err(error)) => {
                    errors.push(error.to_string());
                    break;
                }
                Err(error) => {
                    errors.push(format!("post-commit sidecar worker failed: {error}"));
                    break;
                }
            }
        }
    }
    let sidecar_ms = sidecar_started.elapsed().as_millis() as u64;

    let csl_started = Instant::now();
    let csl_manager = match job.csl_manager.take() {
        None => None,
        Some(mut manager) => match manager
            .persist_turn(job.turn, &job.final_messages, &job.csl_state)
            .await
        {
            Ok(()) => Some(manager),
            Err(error) => {
                errors.push(format!(
                    "CSL projection failed; journal continuation remains available: {error}"
                ));
                // Keep the manager: a transient append failure must not make
                // all later turns silently stop producing CSL projections.
                Some(manager)
            }
        },
    };
    let csl_ms = csl_started.elapsed().as_millis() as u64;

    let notification_started = Instant::now();
    if let Some((config, elapsed)) = job.notification.take() {
        if tokio::time::timeout(
            Duration::from_millis(250),
            notifications::notify_completion(&config, "Astra", "Turn completed", elapsed),
        )
        .await
        .is_err()
        {
            tracing::debug!("completion notification exceeded deferred post-commit budget");
        }
    }
    let notification_ms = notification_started.elapsed().as_millis() as u64;
    let total_ms = started.elapsed().as_millis() as u64;
    if total_ms >= 5_000 {
        tracing::warn!(
            target: "astra_cli::turn_post_commit",
            session_id = ?session_id,
            turn,
            total_ms,
            sidecar_ms,
            csl_ms,
            notification_ms,
            error_count = errors.len(),
            "deferred turn projection completed slowly"
        );
    } else {
        tracing::debug!(
            target: "astra_cli::turn_post_commit",
            session_id = ?session_id,
            turn,
            total_ms,
            sidecar_ms,
            csl_ms,
            notification_ms,
            error_count = errors.len(),
            "deferred turn projection completed"
        );
    }
    TurnPostCommitCompletion {
        owner_id,
        session_id,
        attachment_epoch,
        expected_cursor,
        csl_manager,
        errors,
    }
}

/// Applies a worker completion to the current live session. A completion from
/// a prior session is intentionally ignored: its journal is already durable,
/// but it must never reattach a stale CSL manager after resume/fork/rebind.
pub(crate) fn apply_turn_post_commit_completion(
    completion: TurnPostCommitCompletion,
    state: &mut SessionState,
) -> Vec<String> {
    let current_owner_id = astra_services::local_owner_scope().id().to_string();
    let current_cursor = state
        .active_conversation
        .as_ref()
        .map(|conversation| conversation.cursor());
    if completion.owner_id != current_owner_id
        || completion.session_id != state.session_id
        || completion.attachment_epoch != state.session_attachment_epoch
        || completion.expected_cursor.as_ref() != current_cursor
    {
        tracing::debug!(
            completed_owner_id = %completion.owner_id,
            current_owner_id = %current_owner_id,
            completed_session_id = ?completion.session_id,
            current_session_id = ?state.session_id,
            completed_attachment_epoch = completion.attachment_epoch,
            current_attachment_epoch = state.session_attachment_epoch,
            completed_cursor = ?completion.expected_cursor,
            current_cursor = ?current_cursor,
            "ignored stale deferred turn-post-commit completion"
        );
        return Vec::new();
    }
    if let Some(manager) = completion.csl_manager {
        // The worker is serialized. Replacing an older manager with its newer
        // completion preserves the latest CSL sequence after rapid turns.
        state.csl_manager = Some(manager);
    }
    if !completion.errors.is_empty() {
        state.session_persistence_error = Some(completion.errors.join("; "));
    }
    completion.errors
}

fn notification_for_turn(
    state: &SessionState,
    elapsed: std::time::Duration,
) -> Option<(notifications::NotificationConfig, Duration)> {
    let notif_config = notifications::NotificationConfig {
        enabled: state.notifications_enabled,
        method: state.notification_method,
        min_duration_secs: state.notification_threshold_secs,
    };
    (notif_config.enabled && notif_config.exceeds_threshold(elapsed))
        .then_some((notif_config, elapsed))
}

pub(crate) fn extract_csl_fields_from_result(_result: &StreamResult) -> CslCheckpointFields {
    CslCheckpointFields
}

#[cfg(test)]
mod tests {
    use super::{
        TurnPostCommitCompletion, apply_turn_post_commit_completion,
        build_full_session_state_compact, execute_turn_post_commit_job,
        extract_csl_fields_from_result, prepare_turn_post_commit_job,
    };
    use crate::cli::session::session_projection::CslCheckpointFields;
    use crate::cli::session::session_state::SessionState;
    use astra_turn_core::conversation_log::{AppendMeta, CslEntry, CslStore, CslStoreError};
    use std::time::Instant;

    fn test_api() -> astra_thin_client::ThinClient {
        astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap()
    }

    struct DelayedCslStore(std::time::Duration);

    #[async_trait::async_trait]
    impl CslStore for DelayedCslStore {
        async fn append(
            &self,
            _session_id: &str,
            _entry: &CslEntry,
            _meta: &AppendMeta,
        ) -> Result<(), CslStoreError> {
            tokio::time::sleep(self.0).await;
            Ok(())
        }

        async fn load_from_latest_snapshot(
            &self,
            _session_id: &str,
        ) -> Result<Vec<CslEntry>, CslStoreError> {
            Ok(Vec::new())
        }

        async fn load_after(
            &self,
            _session_id: &str,
            _after_seq: u64,
        ) -> Result<Vec<CslEntry>, CslStoreError> {
            Ok(Vec::new())
        }

        async fn truncate_before(
            &self,
            _session_id: &str,
            _before_seq: u64,
        ) -> Result<u64, CslStoreError> {
            Ok(0)
        }

        async fn fork(
            &self,
            _parent_session_id: &str,
            _new_session_id: &str,
            _fork_after_turn: u32,
        ) -> Result<u64, CslStoreError> {
            Ok(0)
        }
    }

    #[test]
    fn extract_csl_fields_from_result_without_checkpoint_returns_empty_projection() {
        let result = crate::tests::stub_stream_result("done");

        let state = SessionState {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };
        let compact = build_full_session_state_compact(
            &state,
            extract_csl_fields_from_result(&result),
            &Default::default(),
        );

        assert_eq!(compact.recent_tools, vec!["bash".to_string()]);
        assert!(compact.blocked_tools.is_empty());
        assert!(compact.approval_overrides.is_none());
        assert!(compact.interruption.is_none());
        assert!(compact.compaction_tracker.is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn csl_projection_uses_the_committed_active_conversation() {
        use astra_turn_core::conversation_log::{
            CslStore, file_store::FileCslStore, manager::CslManager,
        };

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-canonical-{}", uuid::Uuid::new_v4());
        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ));
        let mgr = CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();
        let mut state = SessionState {
            session_id: Some(session_id.clone()),
            csl_manager: Some(mgr),
            turn: 1,
            ..Default::default()
        };
        let mut objective = serde_json::json!({"role": "user", "content": "old review"});
        astra_turn_types::mark_user_turn_semantics(
            &mut objective,
            astra_turn_types::UserTurnSemantics::new(
                astra_turn_types::ObjectiveRelation::Replace,
                None,
            ),
        );
        let final_messages = vec![
            objective,
            serde_json::json!({"role": "system", "content": "arbitrary compaction boundary", "_compact_boundary": true}),
            serde_json::json!({"role": "user", "content": "不要review啊！"}),
            serde_json::json!({"role": "assistant", "reasoning_content": "trace"}),
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "tool output"}),
            serde_json::json!({"role": "assistant", "content": "ok"}),
        ];
        let canonical_messages =
            astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(
                final_messages.clone(),
            )
            .unwrap();
        state.active_conversation = Some(
            astra_turn_core::active_conversation::ActiveConversation::from_projection(
                astra_services::local_owner_scope().id(),
                &session_id,
                canonical_messages.clone(),
                1,
                astra_turn_core::active_conversation::ActiveConversationSource::Live,
            )
            .unwrap(),
        );

        let api = test_api();
        let job = prepare_turn_post_commit_job(
            &mut state,
            &api,
            None,
            final_messages,
            CslCheckpointFields,
            Instant::now(),
        );
        let completion = execute_turn_post_commit_job(job).await;
        assert!(completion.errors.is_empty(), "{:?}", completion.errors);
        apply_turn_post_commit_completion(completion, &mut state);

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        let mat = astra_turn_core::conversation_log::materialize(&entries).unwrap();
        assert_eq!(mat.messages, canonical_messages);
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.messages[0]["content"], "不要review啊！");
        assert_eq!(mat.messages[1]["content"], "ok");
    }

    #[tokio::test]
    async fn delayed_csl_projection_completes_without_being_cancelled() {
        use astra_turn_core::conversation_log::manager::CslManager;

        let session_id = format!("csl-projection-budget-{}", uuid::Uuid::new_v4());
        let manager = CslManager::new(
            std::sync::Arc::new(DelayedCslStore(std::time::Duration::from_millis(30))),
            session_id.clone(),
            Default::default(),
        )
        .unwrap();
        let mut state = SessionState {
            session_id: Some(session_id),
            csl_manager: Some(manager),
            turn: 1,
            ..Default::default()
        };

        let api = test_api();
        let job = prepare_turn_post_commit_job(
            &mut state,
            &api,
            None,
            Vec::new(),
            CslCheckpointFields,
            Instant::now(),
        );
        let completion = execute_turn_post_commit_job(job).await;
        assert!(completion.errors.is_empty(), "{:?}", completion.errors);
        assert!(completion.csl_manager.is_some());
    }

    #[test]
    fn preparing_deferred_projection_keeps_active_manager_available() {
        let session_id = format!("csl-active-manager-{}", uuid::Uuid::new_v4());
        let manager = astra_turn_core::conversation_log::manager::CslManager::new(
            std::sync::Arc::new(DelayedCslStore(std::time::Duration::ZERO)),
            session_id.clone(),
            Default::default(),
        )
        .unwrap();
        let mut state = SessionState {
            session_id: Some(session_id),
            csl_manager: Some(manager),
            ..Default::default()
        };

        let _job = prepare_turn_post_commit_job(
            &mut state,
            &test_api(),
            None,
            Vec::new(),
            CslCheckpointFields,
            Instant::now(),
        );
        assert!(state.csl_manager.is_some());
    }

    #[test]
    fn deferred_completion_cannot_cross_an_attachment_epoch() {
        let active = astra_turn_core::active_conversation::ActiveConversation::empty(
            astra_services::local_owner_scope().id(),
            "session-a",
        )
        .unwrap()
        .prepare_commit(
            1,
            None,
            vec![serde_json::json!({"role": "user", "content": "one"})],
        )
        .unwrap()
        .next;
        let expected_cursor = active.cursor().clone();
        let mut state = SessionState {
            session_id: Some("session-a".to_string()),
            session_attachment_epoch: 7,
            active_conversation: Some(active),
            ..Default::default()
        };
        let completion = TurnPostCommitCompletion {
            owner_id: astra_services::local_owner_scope().id().to_string(),
            session_id: Some("session-a".to_string()),
            attachment_epoch: 6,
            expected_cursor: Some(expected_cursor),
            csl_manager: None,
            errors: vec!["stale projection error".to_string()],
        };

        assert!(apply_turn_post_commit_completion(completion, &mut state).is_empty());
        assert!(
            state.session_persistence_error.is_none(),
            "a stale completion must not mutate the newly attached session"
        );
    }
}
