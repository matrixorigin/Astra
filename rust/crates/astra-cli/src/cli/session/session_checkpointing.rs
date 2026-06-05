use super::*;
use crate::cli::session::session_recovery;
use crate::cli::session::session_side_effects::enqueue_ingestion_pub;

/// User-initiated checkpoint: heavy JSON + composite index first, then session markdown,
/// journal, and workspace — avoids workspace/checkpoint markdown ahead of failed heavy writes.
#[derive(Debug, Clone)]
pub(crate) struct ManualCheckpointSummary {
    pub checkpoint_number: u32,
    pub turn: u32,
    pub checkpoint_path: std::path::PathBuf,
    pub heavy_path: std::path::PathBuf,
    pub cloud_sync_queued: bool,
}

impl ManualCheckpointSummary {
    pub fn headline(&self) -> String {
        format!(
            "Checkpoint #{} saved (turn {})",
            self.checkpoint_number, self.turn
        )
    }
}

/// After heavy JSON exists: bump workspace checkpoint list, write markdown, journal, ingestion, `workspace.yaml`.
fn persist_manual_session_checkpoint_layer(
    state: &SessionState,
    journal: &session_journal::JournalWriter,
    sid: &str,
    ws: &mut astra_services::session_workspace::WorkspaceMetadata,
    title: &str,
) -> Result<
    (
        std::path::PathBuf,
        u32,
        astra_services::session_checkpoint::Checkpoint,
    ),
    String,
> {
    ws.record_checkpoint();
    let cp_number = ws.checkpoints.len() as u32;
    let summary = format!(
        "User /checkpoint at turn {} — {} ({} turns in history, {} recent tools).",
        ws.turn_count,
        title,
        state.history.len(),
        state.recent_tools.len(),
    );

    let checkpoint = astra_services::session_checkpoint::Checkpoint {
        number: cp_number,
        turn: ws.turn_count,
        title: title.to_string(),
        summary: summary.clone(),
        tools_used: state.recent_tools.clone(),
        total_tokens: ws.total_tokens_in + ws.total_tokens_out,
        had_stalls: false,
        error_count: 0,
        contract_state_json: state
            .durable_task_state
            .as_ref()
            .and_then(|durable| serde_json::to_string(&durable.contract).ok()),
    };

    let checkpoint_path = astra_services::session_checkpoint::write_checkpoint(sid, &checkpoint)
        .map_err(|error| format!("write session checkpoint: {error}"))?;

    let checkpoint_event = session_journal::JournalEvent::checkpoint(
        Some(sid),
        ws.turn_count,
        &summary,
        ws.total_tokens_in + ws.total_tokens_out,
        state.recent_tools.len(),
    );
    if let Err(error) = journal.append(&checkpoint_event) {
        astra_core::agent_warn!(
            "checkpoint",
            "journal append failed after writing session checkpoint markdown (file={}): {error}",
            checkpoint_path.display()
        );
        return Err(format!(
            "journal append failed (checkpoint markdown exists at {}): {error}",
            checkpoint_path.display()
        ));
    }
    enqueue_ingestion_pub(state, &checkpoint_event);

    astra_services::session_workspace::write_workspace(ws)
        .map_err(|error| format!("write workspace: {error}"))?;

    Ok((checkpoint_path, cp_number, checkpoint))
}

/// Queue session + step checkpoint uploads (best-effort; errors only in logs).
fn spawn_manual_checkpoint_cloud_uploads(
    state: &SessionState,
    sid: &str,
    session_cp: &astra_services::session_checkpoint::Checkpoint,
    next_step: u32,
    turn: u32,
    title: &str,
    step_cp: &astra_pipeline::step_protocol::StepCheckpoint,
) {
    let _ = (state, sid, session_cp, next_step, turn, title, step_cp);
}

pub(crate) fn create_manual_checkpoint(
    state: &mut SessionState,
    label_arg: &str,
) -> Result<ManualCheckpointSummary, String> {
    let sid = state
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| "No active session — chat once first.".to_string())?;
    let journal = state
        .journal
        .as_ref()
        .ok_or_else(|| "Journal not available.".to_string())?;

    let title = match label_arg.trim() {
        "" => "Manual checkpoint".to_string(),
        trimmed => trimmed.to_string(),
    };

    let mut workspace = session_recovery::workspace_metadata_from_live_state(state, sid);
    let next_step = session_recovery::next_step_checkpoint_number(sid)?;
    let previous_heavy = astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(sid)
        .map_err(|error| format!("read latest heavy checkpoint: {error}"))?;
    let session_state = previous_heavy
        .as_ref()
        .map(session_recovery::session_state_compact_from_heavy_checkpoint)
        .or_else(|| {
            state.csl_manager.as_ref().and_then(|manager| {
                (manager.last_seq() > 0).then(|| manager.last_session_state().clone())
            })
        })
        .unwrap_or_default();
    let step_checkpoint = session_recovery::build_manual_heavy_step_checkpoint(
        state,
        sid,
        &session_state,
        previous_heavy.as_ref(),
    );
    let heavy_path = session_recovery::persist_manual_heavy_and_composite(
        sid,
        workspace.turn_count,
        &title,
        next_step,
        &step_checkpoint,
    )?;

    let turn = workspace.turn_count;
    let (checkpoint_path, checkpoint_number, checkpoint) =
        persist_manual_session_checkpoint_layer(state, journal, sid, &mut workspace, &title)?;

    spawn_manual_checkpoint_cloud_uploads(
        state,
        sid,
        &checkpoint,
        next_step,
        turn,
        &title,
        &step_checkpoint,
    );

    Ok(ManualCheckpointSummary {
        checkpoint_number,
        turn,
        checkpoint_path,
        heavy_path,
        cloud_sync_queued: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_pipeline::step_protocol::StepCheckpoint;

    fn isolated_sessions_dir() -> (tempfile::TempDir, session_journal::JournalDirGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let guard = session_journal::JournalDirGuard::new(&sessions);
        (tmp, guard)
    }

    fn workspace_backup_path_for(session_id: &str) -> Option<std::path::PathBuf> {
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(session_id);
        std::fs::read_dir(workspace_dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("workspace.yaml.corrupt-"))
            })
    }

    #[test]
    fn persist_manual_session_checkpoint_layer_writes_md_journal_workspace() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();

        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        workspace.turn_count = 3;
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let journal = session_journal::JournalWriter::new(&sid).unwrap();
        let mut state = SessionState::default();
        state.history.push(("hi".into(), "hello".into()));
        state.recent_tools = vec!["read_file".to_string()];

        let mut workspace = astra_services::session_workspace::read_workspace(&sid).unwrap();
        let (checkpoint_path, checkpoint_number, _checkpoint) =
            persist_manual_session_checkpoint_layer(
                &state,
                &journal,
                &sid,
                &mut workspace,
                "decision A",
            )
            .unwrap();

        assert_eq!(checkpoint_number, 1);
        assert!(checkpoint_path.exists());
        assert_eq!(workspace.checkpoints, vec![3]);

        let journal_text =
            std::fs::read_to_string(session_journal::journal_file_path(&sid)).unwrap();
        assert!(journal_text.contains("\"checkpoint\"") || journal_text.contains("checkpoint"));
        assert!(journal_text.contains("decision"));

        let updated_workspace = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(updated_workspace.checkpoints, vec![3]);
    }

    #[test]
    fn spawn_manual_cloud_uploads_no_panic_without_matrix() {
        let state = SessionState::default();
        let checkpoint = astra_services::session_checkpoint::Checkpoint {
            number: 1,
            turn: 1,
            title: "t".into(),
            summary: "s".into(),
            tools_used: vec![],
            total_tokens: 0,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        let step_checkpoint = session_recovery::build_manual_heavy_step_checkpoint(
            &state,
            "noop",
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
            None,
        );
        spawn_manual_checkpoint_cloud_uploads(
            &state,
            "noop",
            &checkpoint,
            1,
            1,
            "t",
            &step_checkpoint,
        );
    }

    #[test]
    fn create_manual_checkpoint_returns_compact_summary() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let journal = session_journal::JournalWriter::new(&sid).unwrap();

        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        workspace.turn_count = 3;
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let mut state = SessionState {
            session_id: Some(sid.clone()),
            journal: Some(journal),
            model: Some("test-model".to_string()),
            ..Default::default()
        };
        state.history.push(("hi".into(), "hello".into()));

        let summary = create_manual_checkpoint(&mut state, "").unwrap();
        assert_eq!(summary.headline(), "Checkpoint #1 saved (turn 3)");
        assert!(summary.checkpoint_path.exists());
        assert!(summary.heavy_path.exists());
        assert!(!summary.cloud_sync_queued);
    }

    #[test]
    fn create_manual_checkpoint_returns_error_when_step_checkpoint_path_is_invalid() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let journal = session_journal::JournalWriter::new(&sid).unwrap();

        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        workspace.turn_count = 2;
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let session_dir = session_journal::local_sessions_dir().join(&sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("step_checkpoints"), "not-a-directory").unwrap();

        let mut state = SessionState {
            session_id: Some(sid),
            journal: Some(journal),
            model: Some("test-model".to_string()),
            ..Default::default()
        };
        state.history.push(("hi".into(), "hello".into()));

        let error = create_manual_checkpoint(&mut state, "")
            .expect_err("invalid step_checkpoints path should fail");
        assert!(error.contains("list step checkpoints"), "{error}");
    }

    #[test]
    fn create_manual_checkpoint_recovers_from_corrupt_workspace_and_preserves_checkpoint_numbering()
    {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let journal = session_journal::JournalWriter::new(&sid).unwrap();

        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        workspace.turn_count = 3;
        astra_services::session_workspace::write_workspace(&workspace).unwrap();
        let first_checkpoint = astra_services::session_checkpoint::Checkpoint {
            number: 1,
            turn: 3,
            title: "First checkpoint".to_string(),
            summary: "baseline".to_string(),
            tools_used: vec![],
            total_tokens: 100,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        astra_services::session_checkpoint::write_checkpoint(&sid, &first_checkpoint).unwrap();
        let workspace_path = astra_services::session_workspace::workspace_file_path(&sid).unwrap();
        let corrupt_bytes = b":\nnot-valid-yaml".to_vec();
        std::fs::write(&workspace_path, &corrupt_bytes).unwrap();

        let mut state = SessionState {
            session_id: Some(sid.clone()),
            journal: Some(journal),
            model: Some("test-model".to_string()),
            turn: 3,
            total_prompt_tokens: 100,
            total_completion_tokens: 50,
            ..Default::default()
        };
        state.history.push(("hi".into(), "hello".into()));

        let summary = create_manual_checkpoint(&mut state, "manual").unwrap();
        assert_eq!(summary.checkpoint_number, 2);

        let repaired_workspace = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(repaired_workspace.checkpoints, vec![3, 3]);
        let backup =
            workspace_backup_path_for(&sid).expect("corrupt workspace should be backed up");
        assert_eq!(std::fs::read(backup).unwrap(), corrupt_bytes);
    }

    #[test]
    fn create_manual_checkpoint_preserves_previous_heavy_recovery_state() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let journal = session_journal::JournalWriter::new(&sid).unwrap();

        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        workspace.turn_count = 3;
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let previous_heavy = astra_pipeline::step_protocol::HeavyCheckpoint {
            light: astra_pipeline::step_protocol::LightCheckpoint {
                protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
                cursor: Default::default(),
                step_id: "session-turn-3".to_string(),
                task_id: "task-3".to_string(),
                agent_id: sid.clone(),
                progress: 1.0,
                total_tokens: 55,
                created_at: astra_pipeline::step_protocol::epoch_ms(),
            },
            messages: vec![serde_json::json!({"role": "user", "content": "stale"})],
            budget_remaining_tokens: 4321,
            budget_remaining_rounds: 6,
            blocked_tools: vec!["write_file".to_string()],
            recent_tools: vec!["read_file".to_string()],
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: Some(serde_json::json!({"kind": "context_overflow"})),
            approval_overrides: Some(serde_json::json!({"tool": "bash"})),
            consecutive_context_window_errors: 3,
            pipeline_state: Some(serde_json::json!({"ema": 0.6})),
            compaction_state: Some(serde_json::json!({"attempt_count": 4})),
            config_version_id: None,
        };
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &sid,
            1,
            &StepCheckpoint::Heavy(Box::new(previous_heavy)),
        )
        .unwrap();

        let mut state = SessionState {
            session_id: Some(sid.clone()),
            journal: Some(journal),
            model: Some("test-model".to_string()),
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };
        state.history.push(("hi".into(), "hello".into()));

        let summary = create_manual_checkpoint(&mut state, "manual").unwrap();
        assert!(summary.heavy_path.exists());

        let restored = astra_pipeline::step_restore::restore_session(&sid)
            .unwrap()
            .expect("restored session");
        assert_eq!(restored.blocked_tools, vec!["write_file".to_string()]);
        assert_eq!(restored.budget_remaining_tokens, 4321);
        assert_eq!(restored.budget_remaining_rounds, 6);
        assert_eq!(
            restored.interruption,
            Some(serde_json::json!({"kind": "context_overflow"}))
        );
        assert_eq!(
            restored.approval_overrides,
            Some(serde_json::json!({"tool": "bash"}))
        );
        assert_eq!(restored.consecutive_context_window_errors, 3);
        assert_eq!(
            restored.compaction_state,
            Some(serde_json::json!({"attempt_count": 4}))
        );
        assert_eq!(
            restored.pipeline_state,
            Some(serde_json::json!({"ema": 0.6}))
        );
    }
}
